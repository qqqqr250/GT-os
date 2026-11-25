//! A small Redox-inspired scheme namespace: "everything is a URL".
//!
//! Redox exposes system resources as paths such as `file:/etc/passwd` or
//! `net:/http://example.com`. `GTos` uses the same uniform URL namespace, and a
//! ring-3 process can now serve its own scheme: it registers a name and a pipe
//! write end, then receives a `CONNECT` notification and a duplex stream for
//! every `open` of that scheme. Built-in resources still come from the kernel.
//!
//! The built-in schemes are:
//!
//! | Scheme | Example | Source |
//! |--------|---------|--------|
//! | `file` | `file:/README.TXT` | mounted FAT16 data disk |
//! | `net`  | `net:/http://example.com` | the HTTP(S) client |
//! | `sys`  | `sys:/mem`, `sys:/net` | kernel status |
//! | registered | `demo:/hello` | a pipe-backed server in ring 3 |
//!
//! An opened resource can be an in-memory snapshot, an incremental FAT file
//! stream or a pipe. Writable FAT descriptors keep dirty clusters in the kernel's
//! bounded LRU cache; they reach the disk on `fsync`, descriptor close or cache
//! eviction. That keeps the syscall surface tiny while still giving programs a
//! single, uniform API.
//!
//! Descriptors can also name a bounded pipe created by `pipe()`. A pipe keeps a
//! byte stream instead of a snapshot, so a forked server and client can exchange
//! data incrementally. Reads return `ERROR` while an empty pipe still has a
//! writer; they return zero only when every write end has closed.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use bootloader_api::BootInfo;
use spin::Mutex;

use crate::shm::Mapping;

/// Bytes buffered by one pipe, shared by all inherited ends.
const PIPE_CAPACITY: usize = 4096;

/// A retained shared-memory region used as a bounded byte ring.
///
/// Metadata stays in the kernel; ring 3 sees only the ordinary stream fds. The
/// mapping itself outlives the SHM region that created it.
#[derive(Debug)]
struct ShmStream {
    mapping: Mapping,
    read: usize,
    write: usize,
    length: usize,
}

impl ShmStream {
    fn new(mapping: Mapping) -> Self {
        Self {
            mapping,
            read: 0,
            write: 0,
            length: 0,
        }
    }

    /// Bytes waiting to be read.
    fn readable(&self) -> usize {
        self.length
    }

    /// Bytes the ring can still accept.
    fn writable(&self) -> usize {
        self.mapping.len() - self.length
    }

    /// Copies as much of `bytes` as the ring can accept, returning the count.
    fn write(&mut self, bytes: &[u8]) -> usize {
        let capacity = self.mapping.len();
        let count = bytes.len().min(capacity - self.length);
        let mut copied = 0;
        while copied < count {
            let index = (self.write + copied) % capacity;
            let chunk = (capacity - index).min(count - copied);
            if !self.mapping.write(index, &bytes[copied..copied + chunk]) {
                break;
            }
            copied += chunk;
        }
        self.write = (self.write + copied) % capacity;
        self.length += copied;
        copied
    }

    /// Copies buffered bytes into `output`, returning the count.
    fn read(&mut self, output: &mut [u8]) -> usize {
        let capacity = self.mapping.len();
        let count = output.len().min(self.length);
        let mut copied = 0;
        while copied < count {
            let index = (self.read + copied) % capacity;
            let chunk = (capacity - index).min(count - copied);
            if !self
                .mapping
                .read(index, &mut output[copied..copied + chunk])
            {
                break;
            }
            copied += chunk;
        }
        self.read = (self.read + copied) % capacity;
        self.length -= copied;
        copied
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamRole {
    Read,
    Write,
    /// A two-way stream: `pipe` is the inbound side, `peer` the outbound side.
    Duplex,
}

#[derive(Debug)]
struct Pipe {
    bytes: VecDeque<u8>,
    readers: usize,
    writers: usize,
    read_shutdown: bool,
    write_shutdown: bool,
    nonblocking: bool,
}

impl Pipe {
    fn write(&mut self, bytes: &[u8]) -> Result<usize, String> {
        if self.write_shutdown || self.readers == 0 {
            return Err(String::from("broken pipe"));
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut written = 0;
        while written < bytes.len() {
            let free = PIPE_CAPACITY - self.bytes.len();
            if free == 0 {
                break;
            }
            let count = free.min(bytes.len() - written);
            self.bytes
                .extend(bytes[written..written + count].iter().copied());
            written += count;
        }
        if written == 0 {
            return Err(String::from(READ_WOULD_BLOCK));
        }
        Ok(written)
    }

    fn read(&mut self, output: &mut [u8]) -> Result<usize, String> {
        if self.read_shutdown {
            return Ok(0);
        }
        if self.bytes.is_empty() {
            return if self.writers == 0 || self.write_shutdown {
                Ok(0)
            } else {
                Err(String::from(READ_WOULD_BLOCK))
            };
        }
        let count = output.len().min(self.bytes.len());
        for slot in &mut output[..count] {
            *slot = self
                .bytes
                .pop_front()
                .expect("count was bounded by the nonempty buffer");
        }
        Ok(count)
    }
}

/// Total bytes buffered in one direction of a socket pair.
const DATAGRAM_CAPACITY: usize = 4096;
/// Largest single datagram accepted by a socket pair.
const MAX_DATAGRAM_BYTES: usize = 1024;

/// A bounded one-way queue of whole messages.
///
/// Unlike a byte pipe, a read returns exactly one datagram, so message
/// boundaries survive every `write`/`read` pair. Separate reader and writer
/// counts give the receiver EOF once every sending end has closed.
#[derive(Debug)]
struct DatagramQueue {
    messages: VecDeque<Vec<u8>>,
    bytes: usize,
    readers: usize,
    writers: usize,
    read_shutdown: bool,
    write_shutdown: bool,
    nonblocking: bool,
}

impl DatagramQueue {
    fn new() -> Self {
        Self {
            messages: VecDeque::new(),
            bytes: 0,
            readers: 1,
            writers: 1,
            read_shutdown: false,
            write_shutdown: false,
            nonblocking: false,
        }
    }

    /// Size of the next datagram, or zero when none is queued.
    fn readable(&self) -> usize {
        self.messages.front().map_or(0, Vec::len)
    }

    /// Room left for new datagram payload bytes.
    fn writable(&self) -> usize {
        DATAGRAM_CAPACITY.saturating_sub(self.bytes)
    }

    fn send(&mut self, bytes: &[u8]) -> Result<usize, String> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.write_shutdown || self.readers == 0 {
            return Err(String::from("socket peer is closed"));
        }
        if bytes.len() > MAX_DATAGRAM_BYTES {
            return Err(String::from("datagram is too large"));
        }
        if bytes.len() > self.writable() {
            return Err(String::from(READ_WOULD_BLOCK));
        }
        self.bytes += bytes.len();
        self.messages.push_back(bytes.to_vec());
        Ok(bytes.len())
    }

    fn receive(&mut self, output: &mut [u8]) -> Result<usize, String> {
        if self.read_shutdown {
            return Ok(0);
        }
        if self.messages.is_empty() {
            return if self.writers == 0 || self.write_shutdown {
                Ok(0)
            } else {
                Err(String::from(READ_WOULD_BLOCK))
            };
        }
        let message = self
            .messages
            .pop_front()
            .expect("the queue was checked to be nonempty");
        self.bytes = self.bytes.saturating_sub(message.len());
        let count = message.len().min(output.len());
        output[..count].copy_from_slice(&message[..count]);
        Ok(count)
    }
}

/// One end of a `SOCK_SEQPACKET` socket pair.
///
/// It reads from `inbound` and writes to `outbound`, so data sent by one end is
/// only visible to the peer. Cloning models inheritance across `fork`; dropping
/// an end releases its reader and writer counts so peers observe EOF.
#[derive(Debug)]
pub(crate) struct DatagramHandle {
    inbound: Arc<Mutex<DatagramQueue>>,
    outbound: Arc<Mutex<DatagramQueue>>,
    peer_pid: u64,
}

impl DatagramHandle {
    fn pair(peer_pid: u64) -> (Self, Self) {
        let left_to_right = Arc::new(Mutex::new(DatagramQueue::new()));
        let right_to_left = Arc::new(Mutex::new(DatagramQueue::new()));
        (
            Self {
                inbound: Arc::clone(&right_to_left),
                outbound: Arc::clone(&left_to_right),
                peer_pid,
            },
            Self {
                inbound: left_to_right,
                outbound: right_to_left,
                peer_pid,
            },
        )
    }

    fn peer_pid(&self) -> u64 {
        self.peer_pid
    }

    fn send(&self, bytes: &[u8]) -> Result<usize, String> {
        self.outbound.lock().send(bytes)
    }

    fn receive(&self, output: &mut [u8]) -> Result<usize, String> {
        self.inbound.lock().receive(output)
    }

    fn readable(&self) -> usize {
        self.inbound.lock().readable()
    }

    fn writable(&self) -> usize {
        self.outbound.lock().writable()
    }

    fn shutdown(&self, how: u64) -> Result<(), String> {
        match how {
            SHUT_RD => {
                self.inbound.lock().read_shutdown = true;
            }
            SHUT_WR => {
                self.outbound.lock().write_shutdown = true;
            }
            SHUT_RDWR => {
                self.inbound.lock().read_shutdown = true;
                self.outbound.lock().write_shutdown = true;
            }
            _ => return Err(String::from("invalid shutdown how")),
        }
        Ok(())
    }
}

impl Clone for DatagramHandle {
    fn clone(&self) -> Self {
        self.inbound.lock().readers += 1;
        self.outbound.lock().writers += 1;
        Self {
            inbound: Arc::clone(&self.inbound),
            outbound: Arc::clone(&self.outbound),
            peer_pid: self.peer_pid,
        }
    }
}

impl Drop for DatagramHandle {
    fn drop(&mut self) {
        let mut inbound = self.inbound.lock();
        inbound.readers = inbound.readers.saturating_sub(1);
        drop(inbound);
        let mut outbound = self.outbound.lock();
        outbound.writers = outbound.writers.saturating_sub(1);
    }
}

/// One end of a stream. Cloning a descriptor clones this handle and its
/// reference counts; dropping it releases those ends so peers observe EOF.
///
/// A pipe end has either an inbound or an outbound side. A scheme connection is
/// duplex: `pipe` is the side this end reads from and `peer` is the side it
/// writes to.
#[derive(Debug)]
pub(crate) struct StreamHandle {
    pipe: Arc<Mutex<Pipe>>,
    peer: Option<Arc<Mutex<Pipe>>>,
    /// Optional shared-memory ring, shared by both ends of a connection.
    shm: Arc<Mutex<Option<ShmStream>>>,
    role: StreamRole,
    /// Process on the other end of a socketpair or scheme connection.
    peer_pid: Option<u64>,
}

impl StreamHandle {
    fn peer_pid(&self) -> Option<u64> {
        self.peer_pid
    }

    fn shutdown(&self, how: u64) -> Result<(), String> {
        match how {
            SHUT_RD => {
                self.pipe.lock().read_shutdown = true;
            }
            SHUT_WR => match self.role {
                StreamRole::Write => self.pipe.lock().write_shutdown = true,
                StreamRole::Duplex => {
                    self.peer
                        .as_ref()
                        .expect("duplex stream has an outbound side")
                        .lock()
                        .write_shutdown = true;
                }
                StreamRole::Read => return Err(String::from("descriptor is read-only")),
            },
            SHUT_RDWR => {
                self.pipe.lock().read_shutdown = true;
                match self.role {
                    StreamRole::Write => self.pipe.lock().write_shutdown = true,
                    StreamRole::Duplex => {
                        self.peer
                            .as_ref()
                            .expect("duplex stream has an outbound side")
                            .lock()
                            .write_shutdown = true;
                    }
                    StreamRole::Read => {}
                }
            }
            _ => return Err(String::from("invalid shutdown how")),
        }
        Ok(())
    }
}

impl Clone for StreamHandle {
    fn clone(&self) -> Self {
        match self.role {
            StreamRole::Read => self.pipe.lock().readers += 1,
            StreamRole::Write => self.pipe.lock().writers += 1,
            StreamRole::Duplex => {
                self.pipe.lock().readers += 1;
                self.peer
                    .as_ref()
                    .expect("duplex stream has an outbound side")
                    .lock()
                    .writers += 1;
            }
        }
        Self {
            pipe: Arc::clone(&self.pipe),
            peer: self.peer.as_ref().map(Arc::clone),
            shm: Arc::clone(&self.shm),
            role: self.role,
            peer_pid: self.peer_pid,
        }
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        match self.role {
            StreamRole::Read => {
                let mut pipe = self.pipe.lock();
                pipe.readers = pipe.readers.saturating_sub(1);
            }
            StreamRole::Write => {
                let mut pipe = self.pipe.lock();
                pipe.writers = pipe.writers.saturating_sub(1);
            }
            StreamRole::Duplex => {
                let mut pipe = self.pipe.lock();
                pipe.readers = pipe.readers.saturating_sub(1);
                drop(pipe);
                if let Some(peer) = self.peer.as_ref() {
                    let mut peer = peer.lock();
                    peer.writers = peer.writers.saturating_sub(1);
                }
            }
        }
    }
}

/// Incremental state for a descriptor backed by one FAT16 file.
///
/// Unlike snapshot resources, a streamed file keeps no guest-sized copy of the
/// contents: each `read`/`write` walks only the clusters it touches, and the
/// directory entry tracks the current on-disk size.
#[derive(Clone, Debug)]
struct FileStream {
    path: String,
    size: u64,
}

/// Incremental state for a descriptor that streams NUL-separated directory
/// names instead of holding a complete listing in memory.
#[derive(Clone, Debug)]
struct DirStream {
    index: usize,
}

/// A descriptor that turns pending signals into readable `Siginfo` records.
///
/// Signals belong to the process, not the descriptor, so this only stores the
/// set to wait for and the descriptor flags. A child that inherits the fd reads
/// its own pending signals after `fork`.
#[derive(Clone, Debug)]
struct SignalFdHandle {
    mask: u64,
    nonblocking: bool,
}

/// An opened resource: its bytes, a MIME type, and a read/write cursor.
#[derive(Clone, Debug)]
pub struct Resource {
    /// The canonical URL this resource was opened from.
    pub url: String,
    /// A best-effort MIME type, useful to callers that want to display it.
    pub mime: String,
    /// The resource contents.
    pub data: Vec<u8>,
    /// Whether ring 3 may write through this descriptor.
    pub writable: bool,
    /// The FAT path to flush after a write, when this is a disk file.
    pub disk_path: Option<String>,
    /// A datagram socket endpoint, when this descriptor names a socket pair.
    datagram: Option<DatagramHandle>,
    /// A pipe endpoint, when this descriptor names a stream instead of bytes.
    stream: Option<StreamHandle>,
    /// Incremental FAT state, when this descriptor streams a disk file.
    file: Option<FileStream>,
    /// Incremental state, when this descriptor streams directory names.
    dir: Option<DirStream>,
    /// Signal wait set, when this descriptor exposes pending signals.
    signalfd: Option<SignalFdHandle>,
    cursor: usize,
}

/// `lseek` whence values, matching the usual Unix constants.
pub const SEEK_SET: u8 = 0;
pub const SEEK_CUR: u8 = 1;
pub const SEEK_END: u8 = 2;

/// `ioctl`: bytes available to read from a descriptor.
pub const IOCTL_FIONREAD: u64 = 1;
/// `ioctl`: free bytes in a descriptor's outgoing buffer.
pub const IOCTL_FIONWRITE: u64 = 2;
/// `ioctl`: current size of a file descriptor.
pub const IOCTL_FILE_SIZE: u64 = 3;
/// `ioctl`: resize a writable file descriptor to `arg` bytes.
pub const IOCTL_FILE_TRUNCATE: u64 = 4;
/// `ioctl`: flush a writable file descriptor to disk.
pub const IOCTL_FILE_SYNC: u64 = 5;
/// `ioctl`: peer process identifier of a socket/stream connection.
pub const IOCTL_SOCKET_PEER_PID: u64 = 6;

/// `shutdown`: disable further receives.
pub const SHUT_RD: u64 = 0;
/// `shutdown`: disable further sends.
pub const SHUT_WR: u64 = 1;
/// `shutdown`: disable further sends and receives.
pub const SHUT_RDWR: u64 = 2;

/// `fcntl`: get file descriptor flags.
pub const F_GETFD: u64 = 1;
/// `fcntl`: set file descriptor flags.
pub const F_SETFD: u64 = 2;
/// `fcntl`: get file status flags.
pub const F_GETFL: u64 = 3;
/// `fcntl`: set file status flags.
pub const F_SETFL: u64 = 4;
/// Close-on-exec file descriptor flag.
pub const FD_CLOEXEC: u64 = 1;
/// Nonblocking file status flag.
pub const O_NONBLOCK: u64 = 1;
/// Close-on-exec flag accepted at descriptor creation time.
pub const O_CLOEXEC: u64 = 0x8_0000;

/// `poll`: the descriptor is readable.
pub const POLLIN: u16 = 0x0001;
/// `poll`: the descriptor is writable.
pub const POLLOUT: u16 = 0x0004;
/// `poll`: the descriptor is in an error state.
pub const POLLERR: u16 = 0x0008;
/// `poll`: the peer closed the connection.
pub const POLLHUP: u16 = 0x0010;
/// `poll`: the descriptor number is not open.
pub const POLLNVAL: u16 = 0x0020;
/// Internal marker for a read that must wait for more data.
pub const READ_WOULD_BLOCK: &str = "would block";

/// Reports whether a descriptor error means data may arrive later.
#[must_use]
pub fn is_would_block(error: &str) -> bool {
    error == READ_WOULD_BLOCK
}

/// Largest file the `truncate` ioctl will create.
const MAX_FILE_BYTES: usize = 1024 * 1024;

/// Room for one 8.3 name plus its NUL terminator and a little slack.
const DIR_NAME_BUFFER_BYTES: usize = 16;

impl Resource {
    /// Decodes the resource as text, replacing invalid UTF-8 with U+FFFD.
    #[must_use]
    pub fn text(&self) -> String {
        if self.dir.is_some() || self.datagram.is_some() || self.signalfd.is_some() {
            return String::new();
        }
        if let Some(file) = self.file.as_ref() {
            return crate::fat::with_volume(|volume| volume.read(&file.path))
                .flatten()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
        }
        String::from_utf8_lossy(&self.data).into_owned()
    }

    /// Current length of the resource, streamed or snapshotted.
    #[must_use]
    pub fn len(&self) -> u64 {
        if self.dir.is_some() {
            return 0;
        }
        if self.datagram.is_some() || self.signalfd.is_some() {
            return 0;
        }
        match (&self.file, &self.stream) {
            (Some(file), _) => file.size,
            (_, Some(_)) => 0,
            _ => self.data.len() as u64,
        }
    }

    /// Answers a readiness query for `poll`.
    ///
    /// `signal_ready` is computed by the caller from the owning process's
    /// pending set, because signals belong to the process rather than to the
    /// descriptor.
    #[must_use]
    pub(crate) fn poll(&self, events: u16, signal_ready: bool) -> u16 {
        if let Some(socket) = self.datagram.as_ref() {
            return datagram_poll(socket, events);
        }
        if let Some(stream) = self.stream.as_ref() {
            return stream_poll(stream, events);
        }
        if self.signalfd.is_some() {
            return if signal_ready { events & POLLIN } else { 0 };
        }
        // Snapshots, streamed files and directories never block in `read`.
        let mut revents = events & POLLIN;
        if self.writable {
            revents |= events & POLLOUT;
        }
        revents
    }
}

/// The boot information, stored so `sys:/mem` can render the memory map.
static BOOT_INFO: Mutex<Option<&'static BootInfo>> = Mutex::new(None);

/// A ring-3 process serving a scheme name. The notify handle is the kernel's
/// private write end of the server's listen pipe.
struct Registration {
    owner: u64,
    notify: StreamHandle,
}

/// Registered scheme names and their server processes.
static SCHEMES: Mutex<Vec<(String, Registration)>> = Mutex::new(Vec::new());

fn valid_scheme_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 16
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Whether a ring-3 process currently serves `name`.
#[must_use]
pub(crate) fn is_registered(name: &str) -> bool {
    SCHEMES
        .lock()
        .iter()
        .any(|(registered, _)| registered.eq_ignore_ascii_case(name))
}

/// Names of all ring-3 schemes, for VFS listings.
#[must_use]
pub(crate) fn names() -> Vec<String> {
    SCHEMES
        .lock()
        .iter()
        .map(|(name, _)| name.clone())
        .collect()
}

/// Registers `name` to be served by `owner` through a pipe write end.
pub fn register(name: &str, owner: u64, notify: StreamHandle) -> bool {
    if !valid_scheme_name(name) {
        return false;
    }
    let mut schemes = SCHEMES.lock();
    if let Some((_, registration)) = schemes
        .iter_mut()
        .find(|(registered, _)| registered.eq_ignore_ascii_case(name))
    {
        if registration.owner != owner {
            return false;
        }
        registration.notify = notify;
    } else {
        schemes.push((String::from(name), Registration { owner, notify }));
    }
    true
}

/// Unregisters `name` when called by its owning process.
pub fn unregister(name: &str, owner: u64) -> bool {
    let removed = {
        let mut schemes = SCHEMES.lock();
        let mut kept = Vec::with_capacity(schemes.len());
        let mut removed = Vec::new();
        for (registered, registration) in schemes.drain(..) {
            if registered.eq_ignore_ascii_case(name) && registration.owner == owner {
                removed.push(registration.notify);
            } else {
                kept.push((registered, registration));
            }
        }
        *schemes = kept;
        removed
    };
    !removed.is_empty()
}

/// Removes every scheme owned by a process that has exited.
pub fn unregister_owner(owner: u64) {
    let removed = {
        let mut schemes = SCHEMES.lock();
        let mut kept = Vec::with_capacity(schemes.len());
        let mut removed = Vec::new();
        for (name, registration) in schemes.drain(..) {
            if registration.owner == owner {
                removed.push(registration.notify);
            } else {
                kept.push((name, registration));
            }
        }
        *schemes = kept;
        removed
    };
    drop(removed);
}

/// Connects to a registered scheme, returning the caller's duplex endpoint.
///
/// The server receives its own endpoint in its descriptor table plus a
/// `CONNECT <fd> <caller> <url>` notification on its listen pipe. No blocking is
/// needed: both ends are ordinary buffered streams.
pub(crate) fn connect(url: &str, caller: u64) -> Option<StreamHandle> {
    let (scheme, _) = split(url).ok()?;
    let (owner, notify) = {
        let schemes = SCHEMES.lock();
        let (_, registration) = schemes
            .iter()
            .find(|(registered, _)| registered.eq_ignore_ascii_case(scheme))?;
        (registration.owner, registration.notify.clone())
    };

    let owner_pid = crate::process::Pid::from_u64(owner)?;
    let (client, server) = FdTable::duplex_pair(owner, caller);
    let server_fd = crate::process::with_process_mut(owner_pid, |process| {
        process.fds_mut().insert_stream(server).ok()
    })
    .flatten()?;
    let event = format!("CONNECT {server_fd} {caller} {url}\n");
    let written = notify.pipe.lock().write(event.as_bytes()).ok()?;
    if written != event.len() {
        let _ = crate::process::with_process_mut(owner_pid, |process| {
            process.fds_mut().close(server_fd)
        });
        return None;
    }
    Some(client)
}

/// Descriptor numbers 0, 1 and 2 are the console streams used by `write`.
const FIRST_FD: u64 = 3;
const MAX_FDS: usize = 16;

/// One open descriptor: its never-reused number and the resource it names.
#[derive(Clone, Debug)]
struct Entry {
    fd: u64,
    resource: Resource,
    cloexec: bool,
}

/// One process's descriptor table.
///
/// Descriptor numbers are handed out monotonically and are **never reused**.
/// Closing fd 3 and opening something else produces a different number, so a
/// stale descriptor held by a program can never silently alias a later,
/// unrelated resource. This is the same guarantee Redox gets from its
/// `HandleMap::insert`, which assigns ever-increasing file handles.
#[derive(Clone, Debug)]
pub struct FdTable {
    entries: [Option<Entry>; MAX_FDS],
    next_fd: u64,
}

/// Answers the occupancy ioctls for one stream descriptor.
fn stream_ioctl(stream: &StreamHandle, request: u64) -> Result<u64, String> {
    let mut shared = stream.shm.lock();
    match request {
        IOCTL_FIONREAD => {
            if stream.role == StreamRole::Write {
                return Err(String::from("descriptor is write-only"));
            }
            if let Some(buffer) = shared.as_mut() {
                return Ok(buffer.readable() as u64);
            }
            Ok(stream.pipe.lock().bytes.len() as u64)
        }
        IOCTL_FIONWRITE => {
            if stream.role == StreamRole::Read {
                return Err(String::from("descriptor is read-only"));
            }
            if let Some(buffer) = shared.as_mut() {
                return Ok(buffer.writable() as u64);
            }
            let outgoing = stream.peer.as_ref().unwrap_or(&stream.pipe);
            Ok((PIPE_CAPACITY - outgoing.lock().bytes.len()) as u64)
        }
        IOCTL_SOCKET_PEER_PID => stream
            .peer_pid()
            .ok_or_else(|| String::from("descriptor has no peer process")),
        _ => Err(String::from("unsupported stream ioctl")),
    }
}

/// Computes `poll` readiness for one stream endpoint.
fn stream_poll(stream: &StreamHandle, events: u16) -> u16 {
    let mut revents = 0;
    let shared = stream.shm.lock();
    match stream.role {
        StreamRole::Write => {
            let pipe = stream.pipe.lock();
            if events & POLLOUT != 0
                && (shared.as_ref().is_some_and(|buffer| buffer.writable() != 0)
                    || pipe.bytes.len() < PIPE_CAPACITY)
            {
                revents |= POLLOUT;
            }
            if pipe.readers == 0 || pipe.write_shutdown {
                revents |= POLLERR;
            }
        }
        StreamRole::Read => {
            let pipe = stream.pipe.lock();
            if events & POLLIN != 0
                && (shared.as_ref().is_some_and(|buffer| buffer.readable() != 0)
                    || !pipe.bytes.is_empty())
            {
                revents |= POLLIN;
            }
            if pipe.writers == 0 || pipe.write_shutdown {
                revents |= POLLHUP;
            }
        }
        StreamRole::Duplex => {
            let inbound = stream.pipe.lock();
            if events & POLLIN != 0
                && (shared.as_ref().is_some_and(|buffer| buffer.readable() != 0)
                    || !inbound.bytes.is_empty())
            {
                revents |= POLLIN;
            }
            if inbound.writers == 0 || inbound.write_shutdown {
                revents |= POLLHUP;
            }
            if events & POLLOUT != 0 {
                let outgoing = stream.peer.as_ref().unwrap_or(&stream.pipe);
                let outgoing = outgoing.lock();
                if shared.as_ref().is_some_and(|buffer| buffer.writable() != 0)
                    || outgoing.bytes.len() < PIPE_CAPACITY
                {
                    revents |= POLLOUT;
                }
            }
        }
    }
    revents
}

/// Computes `poll` readiness for one datagram socket endpoint.
fn datagram_poll(socket: &DatagramHandle, events: u16) -> u16 {
    let mut revents = 0;
    if events & POLLIN != 0 && socket.readable() != 0 {
        revents |= POLLIN;
    }
    if events & POLLOUT != 0 && socket.writable() != 0 {
        revents |= POLLOUT;
    }
    if socket.inbound.lock().writers == 0 {
        revents |= POLLHUP;
    }
    revents
}

impl FdTable {
    /// Creates an empty table. Fds 0, 1 and 2 are reserved for the console.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: [const { None }; MAX_FDS],
            next_fd: FIRST_FD,
        }
    }

    /// Opens `url` and installs the resource in the next free slot.
    pub fn open(&mut self, url: &str) -> Result<u64, String> {
        let resource = open(url)?;
        Ok(self.insert(resource))
    }

    /// Installs an already-open resource and returns its descriptor number.
    pub fn insert(&mut self, resource: Resource) -> u64 {
        let slot = self
            .entries
            .iter()
            .position(Option::is_none)
            .expect("descriptor table is full");
        let mut fd = self.next_fd;
        while self
            .entries
            .iter()
            .any(|entry| entry.as_ref().is_some_and(|entry| entry.fd == fd))
        {
            fd = fd.wrapping_add(1);
        }
        self.next_fd = fd.wrapping_add(1);
        self.entries[slot] = Some(Entry {
            fd,
            resource,
            cloexec: false,
        });
        fd
    }

    /// Whether `count` more descriptors fit without a panic.
    #[must_use]
    pub fn has_room(&self, count: usize) -> bool {
        self.entries.iter().filter(|slot| slot.is_none()).count() >= count
    }

    /// Creates a bounded byte stream and returns its read and write ends.
    pub fn pipe(&mut self) -> Result<(u64, u64), String> {
        if !self.has_room(2) {
            return Err(String::from("descriptor table is full"));
        }
        let pipe = Arc::new(Mutex::new(Pipe {
            bytes: VecDeque::new(),
            readers: 1,
            writers: 1,
            read_shutdown: false,
            write_shutdown: false,
            nonblocking: false,
        }));
        let read_end = Resource {
            url: String::from("pipe:/read"),
            mime: String::from("application/octet-stream"),
            data: Vec::new(),
            writable: false,
            disk_path: None,
            datagram: None,
            stream: Some(StreamHandle {
                pipe: Arc::clone(&pipe),
                peer: None,
                shm: Arc::new(Mutex::new(None)),
                role: StreamRole::Read,
                peer_pid: None,
            }),
            file: None,
            dir: None,
            signalfd: None,
            cursor: 0,
        };
        let write_end = Resource {
            url: String::from("pipe:/write"),
            mime: String::from("application/octet-stream"),
            data: Vec::new(),
            writable: true,
            disk_path: None,
            datagram: None,
            stream: Some(StreamHandle {
                pipe,
                peer: None,
                shm: Arc::new(Mutex::new(None)),
                role: StreamRole::Write,
                peer_pid: None,
            }),
            file: None,
            dir: None,
            signalfd: None,
            cursor: 0,
        };
        Ok((self.insert(read_end), self.insert(write_end)))
    }

    /// Creates two connected `SOCK_SEQPACKET` endpoints.
    ///
    /// Each end may send and receive whole datagrams. Writes are atomic: a
    /// reader observes one complete message instead of an arbitrary byte slice.
    pub fn socketpair(&mut self, owner_pid: u64) -> Result<(u64, u64), String> {
        self.socketpair_seqpacket(owner_pid)
    }

    /// Creates two connected `SOCK_SEQPACKET` endpoints.
    pub fn socketpair_seqpacket(&mut self, owner_pid: u64) -> Result<(u64, u64), String> {
        if !self.has_room(2) {
            return Err(String::from("descriptor table is full"));
        }
        let (left, right) = DatagramHandle::pair(owner_pid);
        let first = Resource {
            url: String::from("socket:/pair-0"),
            mime: String::from("application/octet-stream"),
            data: Vec::new(),
            writable: true,
            disk_path: None,
            datagram: Some(left),
            stream: None,
            file: None,
            dir: None,
            signalfd: None,
            cursor: 0,
        };
        let second = Resource {
            url: String::from("socket:/pair-1"),
            mime: String::from("application/octet-stream"),
            data: Vec::new(),
            writable: true,
            disk_path: None,
            datagram: Some(right),
            stream: None,
            file: None,
            dir: None,
            signalfd: None,
            cursor: 0,
        };
        Ok((self.insert(first), self.insert(second)))
    }

    /// Creates two connected `SOCK_STREAM` endpoints.
    ///
    /// Unlike seqpacket sockets, stream sockets are a byte stream: writes may be
    /// coalesced and reads may return partial data.
    pub fn socketpair_stream(&mut self, owner_pid: u64) -> Result<(u64, u64), String> {
        if !self.has_room(2) {
            return Err(String::from("descriptor table is full"));
        }
        let (left, right) = Self::duplex_pair(owner_pid, owner_pid);
        let first = Resource {
            url: String::from("socket:/stream-0"),
            mime: String::from("application/octet-stream"),
            data: Vec::new(),
            writable: true,
            disk_path: None,
            datagram: None,
            stream: Some(left),
            file: None,
            dir: None,
            signalfd: None,
            cursor: 0,
        };
        let second = Resource {
            url: String::from("socket:/stream-1"),
            mime: String::from("application/octet-stream"),
            data: Vec::new(),
            writable: true,
            disk_path: None,
            datagram: None,
            stream: Some(right),
            file: None,
            dir: None,
            signalfd: None,
            cursor: 0,
        };
        Ok((self.insert(first), self.insert(second)))
    }

    /// Creates two connected duplex endpoints for a scheme connection.
    ///
    /// The first endpoint reads client bytes and writes to the server; the
    /// second reads server bytes and writes to the client.
    #[must_use]
    pub(crate) fn duplex_pair(
        client_peer_pid: u64,
        server_peer_pid: u64,
    ) -> (StreamHandle, StreamHandle) {
        let client_to_server = Arc::new(Mutex::new(Pipe {
            bytes: VecDeque::new(),
            readers: 1,
            writers: 1,
            read_shutdown: false,
            write_shutdown: false,
            nonblocking: false,
        }));
        let server_to_client = Arc::new(Mutex::new(Pipe {
            bytes: VecDeque::new(),
            readers: 1,
            writers: 1,
            read_shutdown: false,
            write_shutdown: false,
            nonblocking: false,
        }));
        let shm = Arc::new(Mutex::new(None));
        let client = StreamHandle {
            pipe: Arc::clone(&server_to_client),
            peer: Some(Arc::clone(&client_to_server)),
            shm: Arc::clone(&shm),
            role: StreamRole::Duplex,
            peer_pid: Some(client_peer_pid),
        };
        let server = StreamHandle {
            pipe: client_to_server,
            peer: Some(server_to_client),
            shm,
            role: StreamRole::Duplex,
            peer_pid: Some(server_peer_pid),
        };
        (client, server)
    }

    /// Installs an opaque stream handle returned by the scheme registry.
    pub fn insert_stream(&mut self, handle: StreamHandle) -> Result<u64, String> {
        if !self.has_room(1) {
            return Err(String::from("descriptor table is full"));
        }
        Ok(self.insert(Resource {
            url: String::from("scheme:/connection"),
            mime: String::from("application/octet-stream"),
            data: Vec::new(),
            writable: true,
            disk_path: None,
            datagram: None,
            stream: Some(handle),
            file: None,
            dir: None,
            signalfd: None,
            cursor: 0,
        }))
    }

    /// Installs a signal descriptor for `mask`.
    ///
    /// The mask is validated by the caller; this method only owns descriptor
    /// bookkeeping. `nonblocking` makes empty reads fail instead of blocking,
    /// and `cloexec` marks the descriptor to be closed by `exec`.
    pub fn insert_signalfd(
        &mut self,
        mask: u64,
        nonblocking: bool,
        cloexec: bool,
    ) -> Result<u64, String> {
        if !self.has_room(1) {
            return Err(String::from("descriptor table is full"));
        }
        let fd = self.insert(Resource {
            url: String::from("signalfd:/pending"),
            mime: String::from("application/octet-stream"),
            data: Vec::new(),
            writable: false,
            disk_path: None,
            datagram: None,
            stream: None,
            file: None,
            dir: None,
            signalfd: Some(SignalFdHandle { mask, nonblocking }),
            cursor: 0,
        });
        if let Some(entry) = self
            .entries
            .iter_mut()
            .filter_map(Option::as_mut)
            .find(|entry| entry.fd == fd)
        {
            entry.cloexec = cloexec;
        }
        Ok(fd)
    }

    /// Returns `(mask, nonblocking)` for a signal descriptor.
    #[must_use]
    pub(crate) fn signalfd(&self, fd: u64) -> Option<(u64, bool)> {
        let handle = self
            .entries
            .iter()
            .filter_map(Option::as_ref)
            .find(|entry| entry.fd == fd)?
            .resource
            .signalfd
            .as_ref()?;
        Some((handle.mask, handle.nonblocking))
    }

    /// Answers a readiness query for one descriptor.
    ///
    /// Unknown descriptors report [`POLLNVAL`] instead of failing the whole
    /// call, matching ordinary `poll` behaviour.
    #[must_use]
    pub fn poll(&self, fd: u64, events: u16, signal_ready: bool) -> u16 {
        let Some(entry) = self
            .entries
            .iter()
            .filter_map(Option::as_ref)
            .find(|entry| entry.fd == fd)
        else {
            return POLLNVAL;
        };
        entry.resource.poll(events, signal_ready)
    }

    /// Attaches a retained shared-memory region to a duplex descriptor.
    ///
    /// Both ends of the connection see the same ring. Plain pipe ends are
    /// rejected so a scheme listener cannot accidentally turn into a bulk
    /// transport. A descriptor can be bound only once.
    pub fn bind_shm(&self, fd: u64, mapping: Mapping) -> bool {
        let Some(entry) = self
            .entries
            .iter()
            .filter_map(Option::as_ref)
            .find(|entry| entry.fd == fd)
        else {
            return false;
        };
        let Some(stream) = entry.resource.stream.as_ref() else {
            return false;
        };
        if stream.role != StreamRole::Duplex {
            return false;
        }
        let mut slot = stream.shm.lock();
        if slot.is_some() {
            return false;
        }
        *slot = Some(ShmStream::new(mapping));
        true
    }

    /// Clones the write end of a pipe so it can be registered as a notify path.
    #[must_use]
    pub fn write_handle(&self, fd: u64) -> Option<StreamHandle> {
        let entry = self
            .entries
            .iter()
            .filter_map(Option::as_ref)
            .find(|entry| entry.fd == fd)?;
        let stream = entry.resource.stream.as_ref()?;
        (stream.role == StreamRole::Write).then(|| stream.clone())
    }

    /// Reads up to `out.len()` bytes from a descriptor, advancing its cursor.
    pub fn read(&mut self, fd: u64, out: &mut [u8]) -> Result<usize, String> {
        let entry = self
            .entries
            .iter_mut()
            .filter_map(Option::as_mut)
            .find(|entry| entry.fd == fd)
            .ok_or_else(|| String::from("bad file descriptor"))?;
        let resource = &mut entry.resource;
        if resource.signalfd.is_some() {
            return Err(String::from(
                "signal descriptor requires the wait read path",
            ));
        }
        if let Some(socket) = resource.datagram.as_ref() {
            return socket.receive(out);
        }
        if let Some(stream) = resource.stream.as_ref() {
            if stream.role == StreamRole::Write {
                return Err(String::from("descriptor is write-only"));
            }
            // EOF comes from the inbound pipe's writer count. The shared ring,
            // when present, only stores payload bytes.
            let eof = stream.pipe.lock().writers == 0;
            let mut shared = stream.shm.lock();
            if let Some(buffer) = shared.as_mut() {
                let count = buffer.read(out);
                if count > 0 {
                    return Ok(count);
                }
                return if eof {
                    Ok(0)
                } else {
                    Err(String::from(READ_WOULD_BLOCK))
                };
            }
            return stream.pipe.lock().read(out);
        }
        let dir_index = resource.dir.as_ref().map(|dir| dir.index);
        if let Some(index) = dir_index {
            if out.is_empty() {
                return Ok(0);
            }
            let (count, next) = crate::fat::with_volume(|volume| volume.read_dir_chunk(index, out))
                .flatten()
                .ok_or_else(|| String::from("failed to read directory"))?;
            if count == 0 && next == index && out.len() < DIR_NAME_BUFFER_BYTES {
                return Err(String::from("directory read buffer is too small"));
            }
            if let Some(dir) = resource.dir.as_mut() {
                dir.index = next;
            }
            resource.cursor = next;
            return Ok(count);
        }
        let file_path = resource.file.as_ref().map(|file| file.path.clone());
        if let Some(path) = file_path {
            let offset = u64::try_from(resource.cursor)
                .map_err(|_| String::from("file offset is too large"))?;
            let count = crate::fat::with_volume(|volume| volume.read_at(&path, offset, out))
                .flatten()
                .ok_or_else(|| format!("failed to read file: {path}"))?;
            resource.cursor = resource.cursor.saturating_add(count);
            return Ok(count);
        }
        let start = resource.cursor;
        if start >= resource.data.len() {
            return Ok(0);
        }
        let end = start.saturating_add(out.len()).min(resource.data.len());
        let count = end - start;
        out[..count].copy_from_slice(&resource.data[start..end]);
        resource.cursor = end;
        Ok(count)
    }

    /// Moves a descriptor's cursor, returning its new absolute offset.
    pub fn seek(&mut self, fd: u64, offset: i64, whence: u8) -> Result<u64, String> {
        let entry = self
            .entries
            .iter_mut()
            .filter_map(Option::as_mut)
            .find(|entry| entry.fd == fd)
            .ok_or_else(|| String::from("bad file descriptor"))?;
        if entry.resource.datagram.is_some() {
            return Err(String::from("sockets are not seekable"));
        }
        if entry.resource.stream.is_some() {
            return Err(String::from("pipes are not seekable"));
        }
        if entry.resource.signalfd.is_some() {
            return Err(String::from("signal descriptors are not seekable"));
        }
        if entry.resource.dir.is_some() {
            if whence != SEEK_SET || offset != 0 {
                return Err(String::from("directories only support rewind"));
            }
            if let Some(dir) = entry.resource.dir.as_mut() {
                dir.index = 0;
            }
            entry.resource.cursor = 0;
            return Ok(0);
        }
        let length = i64::try_from(entry.resource.len()).unwrap_or(i64::MAX);
        let base = match whence {
            SEEK_SET => 0,
            SEEK_CUR => entry.resource.cursor as i64,
            SEEK_END => length,
            _ => return Err(String::from("invalid whence")),
        };
        let target = base
            .checked_add(offset)
            .ok_or_else(|| String::from("seek overflow"))?;
        if target < 0 {
            return Err(String::from("negative seek offset"));
        }
        let target = usize::try_from(target).map_err(|_| String::from("seek offset too large"))?;
        entry.resource.cursor = target;
        Ok(target as u64)
    }

    /// Queries or adjusts one descriptor without transferring payload bytes.
    pub fn ioctl(&mut self, fd: u64, request: u64, arg: u64) -> Result<u64, String> {
        let entry = self
            .entries
            .iter_mut()
            .filter_map(Option::as_mut)
            .find(|entry| entry.fd == fd)
            .ok_or_else(|| String::from("bad file descriptor"))?;
        let resource = &mut entry.resource;

        if let Some(socket) = resource.datagram.as_ref() {
            return match request {
                IOCTL_FIONREAD => Ok(socket.readable() as u64),
                IOCTL_FIONWRITE => Ok(socket.writable() as u64),
                IOCTL_SOCKET_PEER_PID => Ok(socket.peer_pid()),
                _ => Err(String::from("unsupported socket ioctl")),
            };
        }
        if let Some(stream) = resource.stream.as_ref() {
            return stream_ioctl(stream, request);
        }

        match request {
            IOCTL_FIONREAD => Ok(resource
                .len()
                .saturating_sub(u64::try_from(resource.cursor).unwrap_or(u64::MAX))),
            IOCTL_FILE_SIZE => Ok(resource.len()),
            IOCTL_FILE_TRUNCATE => {
                if !resource.writable {
                    return Err(String::from("descriptor is not a writable file"));
                }
                let limit = u64::try_from(MAX_FILE_BYTES).unwrap_or(u64::MAX);
                if arg > limit {
                    return Err(String::from("size is too large"));
                }
                let path = resource
                    .file
                    .as_ref()
                    .map(|file| file.path.clone())
                    .or_else(|| resource.disk_path.clone())
                    .ok_or_else(|| String::from("descriptor has no write backing"))?;
                let truncated =
                    crate::fat::with_volume(|volume| volume.truncate(&path, arg)).unwrap_or(false);
                if !truncated {
                    return Err(String::from("failed to truncate file"));
                }
                if let Some(file) = resource.file.as_mut() {
                    file.size = arg;
                } else {
                    resource.data.resize(
                        usize::try_from(arg).map_err(|_| String::from("size is too large"))?,
                        0,
                    );
                }
                resource.cursor = resource
                    .cursor
                    .min(usize::try_from(arg).unwrap_or(usize::MAX));
                Ok(arg)
            }
            IOCTL_FILE_SYNC => {
                if !resource.writable {
                    return Err(String::from("descriptor is read-only"));
                }
                if let Some(file) = resource.file.as_ref() {
                    let path = file.path.clone();
                    let fallback = file.size;
                    let size = crate::fat::with_volume(|volume| volume.metadata(&path))
                        .flatten()
                        .map_or(fallback, |entry| u64::from(entry.size));
                    if let Some(file) = resource.file.as_mut() {
                        file.size = size;
                    }
                    return if crate::fat::sync() {
                        Ok(size)
                    } else {
                        Err(String::from("failed to flush file"))
                    };
                }
                let Some(path) = resource.disk_path.as_ref() else {
                    return Err(String::from("descriptor has no write backing"));
                };
                let flushed = crate::fat::with_volume(|volume| volume.write(path, &resource.data))
                    .unwrap_or(false);
                if flushed && crate::fat::sync() {
                    Ok(resource.data.len() as u64)
                } else {
                    Err(String::from("failed to flush file"))
                }
            }
            _ => Err(String::from("unsupported file ioctl")),
        }
    }

    /// Writes to a writable descriptor, marking FAT clusters dirty for write-back.
    pub fn write(&mut self, fd: u64, bytes: &[u8]) -> Result<usize, String> {
        let entry = self
            .entries
            .iter_mut()
            .filter_map(Option::as_mut)
            .find(|entry| entry.fd == fd)
            .ok_or_else(|| String::from("bad file descriptor"))?;
        if let Some(socket) = entry.resource.datagram.as_ref() {
            return socket.send(bytes);
        }
        if let Some(stream) = entry.resource.stream.as_ref() {
            if stream.role == StreamRole::Read {
                return Err(String::from("descriptor is read-only"));
            }
            let outgoing = stream.peer.as_ref().unwrap_or(&stream.pipe);
            let mut shared = stream.shm.lock();
            if let Some(buffer) = shared.as_mut() {
                if outgoing.lock().readers == 0 {
                    return Err(String::from("broken pipe"));
                }
                return Ok(buffer.write(bytes));
            }
            return outgoing.lock().write(bytes);
        }
        if !entry.resource.writable {
            return Err(String::from("descriptor is read-only"));
        }
        let file_path = entry.resource.file.as_ref().map(|file| file.path.clone());
        if let Some(path) = file_path {
            let offset = u64::try_from(entry.resource.cursor)
                .map_err(|_| String::from("file offset is too large"))?;
            let count =
                u64::try_from(bytes.len()).map_err(|_| String::from("write is too large"))?;
            let end = offset.saturating_add(count);
            let written = crate::fat::with_volume(|volume| volume.write_at(&path, offset, bytes))
                .unwrap_or(false);
            if !written {
                return Err(format!("failed to write file: {path}"));
            }
            if let Some(file) = entry.resource.file.as_mut() {
                file.size = file.size.max(end);
            }
            entry.resource.cursor =
                usize::try_from(end).map_err(|_| String::from("file offset is too large"))?;
            return Ok(bytes.len());
        }
        let Some(path) = entry.resource.disk_path.clone() else {
            return Err(String::from("descriptor has no write backing"));
        };

        let start = entry.resource.cursor;
        let end = start.saturating_add(bytes.len());
        let mut data = core::mem::take(&mut entry.resource.data);
        if end > data.len() {
            data.resize(end, 0);
        }
        data[start..end].copy_from_slice(bytes);
        let flushed = crate::fat::with_volume(|volume| volume.write(&path, &data)).unwrap_or(false);
        entry.resource.data = data;
        if !flushed {
            return Err(String::from("failed to flush file"));
        }
        entry.resource.cursor = end;
        Ok(bytes.len())
    }

    /// Duplicates a descriptor. The new descriptor shares underlying
    /// pipe/socket/SHM handles but has an independent cursor and copied
    /// resource state.
    pub fn dup(&mut self, fd: u64) -> Result<u64, String> {
        let resource = self
            .entries
            .iter()
            .filter_map(Option::as_ref)
            .find(|entry| entry.fd == fd)
            .map(|entry| entry.resource.clone())
            .ok_or_else(|| String::from("bad file descriptor"))?;
        if !self.has_room(1) {
            return Err(String::from("descriptor table is full"));
        }
        Ok(self.insert(resource))
    }

    /// Duplicates `oldfd` to `newfd`, closing `newfd` first if it is open.
    pub fn dup2(&mut self, oldfd: u64, newfd: u64) -> Result<u64, String> {
        if oldfd < FIRST_FD || newfd < FIRST_FD {
            return Err(String::from("bad file descriptor"));
        }
        let resource = self
            .entries
            .iter()
            .filter_map(Option::as_ref)
            .find(|entry| entry.fd == oldfd)
            .map(|entry| entry.resource.clone())
            .ok_or_else(|| String::from("bad file descriptor"))?;
        if oldfd == newfd {
            return Ok(newfd);
        }
        if self
            .entries
            .iter()
            .filter_map(Option::as_ref)
            .all(|entry| entry.fd != newfd)
            && !self.has_room(1)
        {
            return Err(String::from("descriptor table is full"));
        }
        self.close(newfd);
        let slot = self
            .entries
            .iter()
            .position(Option::is_none)
            .ok_or_else(|| String::from("descriptor table is full"))?;
        self.entries[slot] = Some(Entry {
            fd: newfd,
            resource,
            cloexec: false,
        });
        Ok(newfd)
    }

    /// Closes a descriptor, returning whether it was open.
    ///
    /// Closing a FAT-backed descriptor writes back any dirty cached clusters so
    /// ordinary `open`/`write`/`close` programs remain durable without learning
    /// about `fsync`.
    pub fn close(&mut self, fd: u64) -> bool {
        let slot = self
            .entries
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|entry| entry.fd == fd));
        let Some(slot) = slot else {
            return false;
        };
        let fat_backed = slot.as_ref().is_some_and(|entry| {
            entry.resource.file.is_some() || entry.resource.disk_path.is_some()
        });
        let closed = slot.take().is_some();
        if closed && fat_backed {
            let _ = crate::fat::sync();
        }
        closed
    }

    /// Releases every descriptor still open when the process exits.
    pub fn clear(&mut self) {
        let fat_backed = self.entries.iter().any(|slot| {
            slot.as_ref().is_some_and(|entry| {
                entry.resource.file.is_some() || entry.resource.disk_path.is_some()
            })
        });
        self.entries.fill(None);
        if fat_backed {
            let _ = crate::fat::sync();
        }
        // `next_fd` is deliberately not reset: descriptors are never reused.
    }

    /// Shuts down read and/or write sides of a socket/stream descriptor.
    pub fn shutdown(&self, fd: u64, how: u64) -> Result<(), String> {
        let entry = self
            .entries
            .iter()
            .filter_map(Option::as_ref)
            .find(|entry| entry.fd == fd)
            .ok_or_else(|| String::from("bad file descriptor"))?;
        if let Some(socket) = entry.resource.datagram.as_ref() {
            return socket.shutdown(how);
        }
        if let Some(stream) = entry.resource.stream.as_ref() {
            return stream.shutdown(how);
        }
        Err(String::from("descriptor does not support shutdown"))
    }

    /// Reports whether a descriptor is nonblocking.
    ///
    /// Files and other always-ready resources return `false`; the caller only
    /// consults this after a descriptor read reported `READ_WOULD_BLOCK`.
    #[must_use]
    pub fn is_nonblocking(&self, fd: u64) -> bool {
        let Some(entry) = self
            .entries
            .iter()
            .filter_map(Option::as_ref)
            .find(|entry| entry.fd == fd)
        else {
            return false;
        };
        if let Some(socket) = entry.resource.datagram.as_ref() {
            return socket.inbound.lock().nonblocking;
        }
        if let Some(stream) = entry.resource.stream.as_ref() {
            return stream.pipe.lock().nonblocking;
        }
        entry
            .resource
            .signalfd
            .as_ref()
            .is_some_and(|signalfd| signalfd.nonblocking)
    }

    /// Gets or sets file descriptor/status flags for a descriptor.
    pub fn fcntl(&mut self, fd: u64, cmd: u64, arg: u64) -> Result<u64, String> {
        let slot = self
            .entries
            .iter()
            .position(|entry| entry.as_ref().is_some_and(|entry| entry.fd == fd))
            .ok_or_else(|| String::from("bad file descriptor"))?;
        let entry = self.entries[slot]
            .as_ref()
            .ok_or_else(|| String::from("bad file descriptor"))?;
        match cmd {
            F_GETFD => Ok(u64::from(entry.cloexec)),
            F_SETFD => {
                if arg & !FD_CLOEXEC != 0 {
                    return Err(String::from("unsupported file descriptor flags"));
                }
                if let Some(entry) = self.entries[slot].as_mut() {
                    entry.cloexec = arg & FD_CLOEXEC != 0;
                }
                Ok(0)
            }
            F_GETFL => {
                if let Some(socket) = entry.resource.datagram.as_ref() {
                    let nb = socket.inbound.lock().nonblocking;
                    return Ok(if nb { O_NONBLOCK } else { 0 });
                }
                if let Some(stream) = entry.resource.stream.as_ref() {
                    let nb = stream.pipe.lock().nonblocking;
                    return Ok(if nb { O_NONBLOCK } else { 0 });
                }
                if let Some(signalfd) = entry.resource.signalfd.as_ref() {
                    return Ok(if signalfd.nonblocking { O_NONBLOCK } else { 0 });
                }
                Ok(0)
            }
            F_SETFL => {
                if arg & !O_NONBLOCK != 0 {
                    return Err(String::from("unsupported file status flags"));
                }
                let nb = arg & O_NONBLOCK != 0;
                if let Some(socket) = entry.resource.datagram.as_ref() {
                    socket.inbound.lock().nonblocking = nb;
                    socket.outbound.lock().nonblocking = nb;
                    return Ok(0);
                }
                if let Some(stream) = entry.resource.stream.as_ref() {
                    stream.pipe.lock().nonblocking = nb;
                    if let Some(peer) = stream.peer.as_ref() {
                        peer.lock().nonblocking = nb;
                    }
                    return Ok(0);
                }
                if entry.resource.signalfd.is_some() {
                    if let Some(entry) = self.entries[slot].as_mut() {
                        if let Some(signalfd) = entry.resource.signalfd.as_mut() {
                            signalfd.nonblocking = nb;
                        }
                    }
                    return Ok(0);
                }
                Ok(0)
            }
            _ => Err(String::from("unsupported fcntl command")),
        }
    }

    /// Closes descriptors marked close-on-exec, as required by `exec`.
    pub fn close_cloexec(&mut self) {
        let mut fat_backed = false;
        for slot in &mut self.entries {
            let close = slot.as_ref().is_some_and(|entry| entry.cloexec);
            if close {
                if let Some(entry) = slot.as_ref() {
                    fat_backed |=
                        entry.resource.file.is_some() || entry.resource.disk_path.is_some();
                }
                *slot = None;
            }
        }
        if fat_backed {
            let _ = crate::fat::sync();
        }
    }
}

impl Default for FdTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Remembers the boot information for the `sys` scheme.
pub fn init(boot_info: &'static BootInfo) {
    *BOOT_INFO.lock() = Some(boot_info);
}

/// Splits `scheme:/path` into its scheme and path, ignoring leading slashes.
fn split(url: &str) -> Result<(&str, &str), String> {
    let (scheme, path) = url
        .split_once(':')
        .ok_or_else(|| String::from("missing scheme (try file:/README.TXT)"))?;
    Ok((scheme, path.trim_start_matches('/')))
}

/// Opens `url` as a snapshot, file stream, or directory stream.
pub fn open(url: &str) -> Result<Resource, String> {
    let (scheme, path) = split(url)?;
    if scheme.eq_ignore_ascii_case("file") && path.is_empty() {
        return open_root_directory();
    }
    if path.is_empty() {
        return Err(String::from("empty resource path"));
    }
    if scheme.eq_ignore_ascii_case("file") {
        open_file(path)
    } else if scheme.eq_ignore_ascii_case("net") {
        open_net(path)
    } else if scheme.eq_ignore_ascii_case("sys") {
        open_sys(path)
    } else {
        Err(format!("unknown scheme '{scheme}' (try file, net or sys)"))
    }
}

/// Opens the FAT root directory as a stream of NUL-separated names.
fn open_root_directory() -> Result<Resource, String> {
    crate::fat::with_volume(|_| ()).ok_or_else(|| String::from("no FAT16 volume is mounted"))?;
    Ok(Resource {
        url: String::from("file:/"),
        mime: String::from("application/x-directory"),
        data: Vec::new(),
        writable: false,
        disk_path: None,
        datagram: None,
        stream: None,
        file: None,
        dir: Some(DirStream { index: 0 }),
        signalfd: None,
        cursor: 0,
    })
}

/// Opens a file on the mounted FAT16 volume as an incremental stream.
fn open_file(path: &str) -> Result<Resource, String> {
    let existing = crate::fat::with_volume(|volume| volume.metadata(path)).flatten();
    if existing
        .as_ref()
        .is_some_and(crate::fat::Entry::is_directory)
    {
        return Err(format!("not a regular file: {path}"));
    }
    let size = if let Some(entry) = existing {
        u64::from(entry.size)
    } else {
        // `open` doubles as create for disk files. The new descriptor streams
        // from the directory entry, so it needs no preloaded byte buffer.
        let created = crate::fat::with_volume(|volume| volume.write(path, &[])).unwrap_or(false);
        if !created {
            return Err(format!("file not found: {path}"));
        }
        crate::fat::with_volume(|volume| volume.metadata(path))
            .flatten()
            .map_or(0, |entry| u64::from(entry.size))
    };
    let extension = path.rsplit_once('.').map(|(_, extension)| extension);
    let mime = match extension {
        Some(extension) if extension.eq_ignore_ascii_case("TXT") => "text/plain",
        Some(extension) if extension.eq_ignore_ascii_case("WAV") => "audio/wav",
        _ => "application/octet-stream",
    };
    Ok(Resource {
        url: format!("file:/{path}"),
        mime: String::from(mime),
        data: Vec::new(),
        writable: true,
        disk_path: Some(String::from(path)),
        datagram: None,
        stream: None,
        file: Some(FileStream {
            path: String::from(path),
            size,
        }),
        dir: None,
        signalfd: None,
        cursor: 0,
    })
}

/// Fetches a URL through the kernel's HTTP(S) client.
fn open_net(path: &str) -> Result<Resource, String> {
    let target = if path.contains("://") {
        String::from(path)
    } else {
        format!("https://{path}")
    };
    let mut response = crate::net::fetch_url(&target)?;
    let mime = response
        .header("content-type")
        .unwrap_or("text/html")
        .to_string();
    Ok(Resource {
        url: core::mem::take(&mut response.final_url),
        mime,
        data: core::mem::take(&mut response.body),
        writable: false,
        disk_path: None,
        datagram: None,
        stream: None,
        file: None,
        dir: None,
        signalfd: None,
        cursor: 0,
    })
}

/// Builds a `sys:` resource from live kernel state.
fn open_sys(path: &str) -> Result<Resource, String> {
    let text = match path {
        "version" => {
            "GTos 0.1.0\nx86_64 bare-metal kernel\nwritten in Rust, no std, no libc\n".to_string()
        }
        "heap" => {
            let (start, size) = crate::memory::info();
            format!(
                "heap: 0x{start:012x} .. 0x{:012x} ({} KiB)\n",
                start + size,
                size / 1024
            )
        }
        "mem" => {
            let guard = BOOT_INFO.lock();
            let Some(info) = guard.as_ref() else {
                return Err(String::from("boot memory map is unavailable"));
            };
            let mut out = String::from("start            end               size  kind\n");
            for region in info.memory_regions.iter() {
                let size = region.end.saturating_sub(region.start);
                let kind = match region.kind {
                    bootloader_api::info::MemoryRegionKind::Usable => "usable",
                    bootloader_api::info::MemoryRegionKind::Bootloader => "bootloader",
                    _ => "other",
                };
                let _ = core::fmt::Write::write_fmt(
                    &mut out,
                    format_args!(
                        "0x{:012x}  0x{:012x}  {:>8} KiB  {kind}\n",
                        region.start,
                        region.end,
                        size / 1024
                    ),
                );
            }
            out
        }
        "net" => {
            let config = crate::net::address();
            let adapter = crate::net::adapter()
                .map(|info| format!("adapter: {}, {}, IRQ {}\n", info.name, info.bus, info.irq))
                .unwrap_or_default();
            format!(
                "{adapter}address: {}\nmask:    {}\ngateway: {}\ndns:     {}\nsource:  {}\n",
                ip(config.ip),
                ip(config.netmask),
                ip(config.gateway),
                ip(config.dns),
                if config.dhcp {
                    "DHCP lease"
                } else {
                    "static fallback"
                },
            )
        }
        "uptime" => format!(
            "uptime: {} s ({} timer ticks)\n",
            crate::interrupts::uptime_seconds(),
            crate::interrupts::ticks()
        ),
        "help" | "schemes" => String::from(
            "schemes:\n  file:/NAME       file on the FAT16 data disk\n  \
             net:/URL          HTTP or HTTPS resource\n  \
             sys:/version      kernel version\n  \
             sys:/heap         kernel heap range\n  \
             sys:/mem          bootloader memory map\n  \
             sys:/net          network configuration\n  \
             sys:/uptime       time since boot\n  \
             sys:/help         this list\n",
        ),
        other => return Err(format!("unknown sys resource '{other}' (try sys:/help)")),
    };
    Ok(Resource {
        url: format!("sys:/{path}"),
        mime: String::from("text/plain"),
        data: text.into_bytes(),
        writable: false,
        disk_path: None,
        datagram: None,
        stream: None,
        file: None,
        dir: None,
        signalfd: None,
        cursor: 0,
    })
}

/// Formats an IPv4 address without pulling in a formatting dependency.
fn ip(bytes: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn splits_scheme_urls() {
        assert_eq!(split("file:/README.TXT").unwrap(), ("file", "README.TXT"));
        assert_eq!(
            split("net:/http://example.com").unwrap(),
            ("net", "http://example.com")
        );
        assert!(split("README.TXT").is_err());
    }

    #[test_case]
    fn sys_resources_are_readable() {
        let version = open("sys:/version").unwrap();
        assert!(version.text().contains("GTos"));
        assert!(open("sys:/help").unwrap().text().contains("file:/"));
        assert!(open("bogus:/x").is_err());
    }

    #[test_case]
    fn descriptor_lifecycle() {
        let mut table = FdTable::new();
        let fd = table.open("sys:/version").unwrap();
        assert!(fd >= FIRST_FD);
        let mut buffer = [0u8; 16];
        let count = table.read(fd, &mut buffer).unwrap();
        assert!(count > 0);
        assert_eq!(&buffer[..5], b"GTos ");
        assert!(table.close(fd));
        assert!(!table.close(fd));
        assert!(table.read(fd, &mut buffer).is_err());
    }

    #[test_case]
    fn descriptors_are_never_reused() {
        let mut table = FdTable::new();
        let first = table.open("sys:/version").unwrap();
        assert!(table.close(first));

        // A later open must not hand out `first` again: a stale reference to a
        // closed descriptor has to stay invalid, not silently read a new
        // resource that happens to land in the same slot.
        let second = table.open("sys:/net").unwrap();
        assert_ne!(first, second);

        let mut buffer = [0u8; 8];
        assert!(table.read(first, &mut buffer).is_err());
        assert!(table.read(second, &mut buffer).is_ok());
        assert!(table.close(second));
    }

    #[test_case]
    fn tables_have_independent_descriptors() {
        let mut first = FdTable::new();
        let mut second = first.clone();
        let fd = first.open("sys:/version").unwrap();
        assert!(second.read(fd, &mut [0u8; 4]).is_err());
    }
}

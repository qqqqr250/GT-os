//! A small ring-3 session manager for `GTos`.
//!
//! It reads VFS metadata, creates a shell surface, starts the configured
//! autostart applications as child processes, publishes session state through
//! the mounted VFS, and dispatches both shell input and registered service-bus
//! requests. It is clean-room Rust: no Plasma, Qt or foreign runtime.

#![no_std]
#![no_main]

gtos_sys::gtos_heap!();
gtos_sys::gtos_main!(main);

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use gtos_sys::{FramebufferInfo, InputEvent, InputPoll, VFS_FILE};
use gtos_widgets::shell::{
    DesktopShell, LauncherRecord, ShellAction, ShellFocus, ShellLayout, ShellSessionAction,
    TrayItem, parse_launcher_records,
};
use gtos_widgets::{Event, Rect, Style, Surface, Theme, Widget};

const APPS_PATH: &[u8] = b"/APPS.TSV";
const AUTOSTART_PATH: &[u8] = b"/AUTO.TSV";
const RUNNERS_PATH: &[u8] = b"/RUNNERS.TSV";
const KCMS_PATH: &[u8] = b"/KCMS.TSV";
const SERVICES_PATH: &[u8] = b"/SERVICES.TSV";
const STATE_PATH: &[u8] = b"file:/SESSION.ST";
const SERVICE_STATE_PATH: &[u8] = b"file:/SERVICES.ST";
const MAX_METADATA_BYTES: usize = 1024;
const MAX_LAUNCH_BYTES: usize = 1 << 20;
const ENTER: u32 = 13;
const HOST_ENTER: u32 = 10;
const SERVICE_NAME: &[u8] = b"session";
const SERVICE_PREFIX: &[u8] = b"session:/";
const SERVICE_TIMEOUT_NS: u64 = 5_000_000_000;
const MAX_REQUEST_BYTES: usize = 128;
const SERVICE_STATE_LINE_WIDTH: usize = 33;
const MAX_RESTARTS: u8 = 9;

const STARTING: &[u8] = b"session manager starting\r\n";
const AUTOSTART_OK: &[u8] = b"session autostart ok\r\n";
const BUS_OK: &[u8] = b"session bus ok\r\n";
const SERVICES_OK: &[u8] = b"session services ok\r\n";
const KCM_OK: &[u8] = b"session kcm ok\r\n";
const WAITING: &[u8] = b"session waiting for input\r\n";
const SHUTDOWN_OK: &[u8] = b"session manager shutdown ok\r\n";
const START_FAILED: &[u8] = b"session start failed\r\n";
const AUTOSTART_FAILED: &[u8] = b"session autostart failed\r\n";
const BUS_FAILED: &[u8] = b"session bus failed\r\n";
const SERVICES_FAILED: &[u8] = b"session services failed\r\n";
const KCM_FAILED: &[u8] = b"session kcm failed\r\n";
const INPUT_FAILED: &[u8] = b"session input failed\r\n";
const DISPATCH_FAILED: &[u8] = b"session dispatch failed\r\n";
const SHUTDOWN_FAILED: &[u8] = b"session shutdown failed\r\n";

/// Fixed-width state records avoid leaving stale trailing bytes in FAT files.
const STATE_STARTING: &[u8] = b"starting\n";
const STATE_RUNNING: &[u8] = b"running \n";
const STATE_STOPPING: &[u8] = b"stopping\n";

/// One managed service declaration from VFS metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ServiceRecord {
    id: Vec<u8>,
    label: Vec<u8>,
    target: Vec<u8>,
    restart_limit: u8,
}

/// Lifecycle state of a managed service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServiceState {
    Running,
    Stopped,
}

impl ServiceState {
    fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Debug)]
struct ManagedService {
    record: ServiceRecord,
    state: ServiceState,
    pid: u64,
    restarts: u8,
}

struct SessionState<'a> {
    runners: &'a [LauncherRecord],
    kcms: &'a [LauncherRecord],
    services: &'a mut [ManagedService],
    children: &'a mut Vec<u64>,
}

const TRAY_ITEMS: &[TrayItem<'static>] = &[
    TrayItem::new(b"NET", false),
    TrayItem::session(b"PWR", ShellSessionAction::Shutdown),
];

fn fail(message: &[u8]) -> ! {
    let _ = gtos_sys::write(1, message);
    gtos_sys::exit(1);
}

fn trim_eol(mut line: &[u8]) -> &[u8] {
    while line.last() == Some(&b'\n') || line.last() == Some(&b'\r') {
        line = &line[..line.len() - 1];
    }
    line
}

fn parse_decimal(bytes: &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
    }
    (!bytes.is_empty()).then_some(value)
}

fn ascii(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("?")
}

fn push_padded(out: &mut Vec<u8>, bytes: &[u8], width: usize) {
    let used = bytes.len().min(width);
    out.extend_from_slice(&bytes[..used]);
    for _ in used..width {
        out.push(b' ');
    }
}

fn push_number(out: &mut Vec<u8>, value: u64, width: usize) {
    let mut buffer = [0u8; 20];
    let mut index = buffer.len();
    let mut remaining = value;
    loop {
        index -= 1;
        let digit = u8::try_from(remaining % 10).unwrap_or(0);
        buffer[index] = b'0' + digit;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    let digits = &buffer[index..];
    for _ in 0..width.saturating_sub(digits.len()) {
        out.push(b' ');
    }
    out.extend_from_slice(digits);
}

/// Reads a small VFS file whose size is reported by `stat`.
fn read_vfs_file(path: &[u8], max_len: usize) -> Option<Vec<u8>> {
    let info = gtos_sys::stat(path)?;
    if info.kind != VFS_FILE || info.size == 0 {
        return None;
    }
    let len = usize::try_from(info.size).ok()?;
    if len == 0 || len > max_len {
        return None;
    }
    let fd = gtos_sys::open(path);
    if fd == gtos_sys::ERROR {
        return None;
    }
    let mut bytes = alloc::vec![0u8; len];
    let mut filled = 0usize;
    let mut ok = true;
    while filled < len {
        let count = gtos_sys::read(fd, &mut bytes[filled..]);
        if count == gtos_sys::ERROR {
            ok = false;
            break;
        }
        match usize::try_from(count) {
            Ok(count) if count > 0 => filled += count,
            _ => {
                ok = false;
                break;
            }
        }
    }
    let _ = gtos_sys::close(fd);
    (ok && filled == len).then_some(bytes)
}

/// Parses clean-room autostart lines: one launcher id per line.
fn parse_autostart(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut ids = Vec::new();
    for raw in bytes.split(|&byte| byte == b'\n') {
        let mut line = raw;
        while line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        ids.push(line.to_vec());
    }
    ids
}

/// Parses service metadata: `id<TAB>label<TAB>target<TAB>restart_limit`.
fn parse_services(bytes: &[u8]) -> Vec<ServiceRecord> {
    let mut records = Vec::new();
    for raw in bytes.split(|&byte| byte == b'\n') {
        let mut line = raw;
        while line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let mut parts = line.split(|&byte| byte == b'\t');
        let (Some(id), Some(label), Some(target), Some(limit)) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next().and_then(parse_decimal),
        ) else {
            continue;
        };
        if id.is_empty() || label.is_empty() || target.is_empty() {
            continue;
        }
        let restart_limit = u8::try_from(limit)
            .unwrap_or(MAX_RESTARTS)
            .min(MAX_RESTARTS);
        records.push(ServiceRecord {
            id: id.to_vec(),
            label: label.to_vec(),
            target: target.to_vec(),
            restart_limit,
        });
    }
    records
}

/// Publishes the current session state through the mounted VFS.
fn write_state(state: &[u8]) -> bool {
    let fd = gtos_sys::open(STATE_PATH);
    if fd == gtos_sys::ERROR {
        return false;
    }
    let written = gtos_sys::write(fd, state);
    let _ = gtos_sys::close(fd);
    written == state.len() as u64
}

/// Publishes fixed-width managed-service records through the mounted VFS.
fn write_service_state(services: &[ManagedService]) -> bool {
    let mut bytes = Vec::with_capacity(services.len() * SERVICE_STATE_LINE_WIDTH);
    for service in services {
        push_padded(&mut bytes, &service.record.id, 12);
        bytes.push(b' ');
        push_padded(&mut bytes, service.state.name().as_bytes(), 8);
        bytes.push(b' ');
        push_number(&mut bytes, service.pid, 7);
        bytes.push(b' ');
        push_number(&mut bytes, u64::from(service.restarts), 2);
        bytes.push(b'\n');
    }
    let fd = gtos_sys::open(SERVICE_STATE_PATH);
    if fd == gtos_sys::ERROR {
        return false;
    }
    let ok = write_all(fd, &bytes);
    let _ = gtos_sys::close(fd);
    ok
}

/// Starts one VFS metadata target and returns its child process identifier.
fn spawn_target(target: &[u8]) -> Option<u64> {
    let image = read_vfs_file(target, MAX_LAUNCH_BYTES)?;
    let child = gtos_sys::fork();
    if child == gtos_sys::ERROR {
        return None;
    }
    if child == 0 {
        gtos_sys::exec(&image);
        gtos_sys::exit(1);
    }
    Some(child)
}

fn reap_children(children: &mut Vec<u64>) {
    let mut index = 0;
    while index < children.len() {
        let mut status = 0;
        if gtos_sys::waitpid(children[index], &mut status) == children[index] {
            children.remove(index);
        } else {
            index += 1;
        }
    }
}

fn paint_shell(surface: &mut Surface, shell: &DesktopShell<'_>) -> bool {
    {
        let mut canvas = surface.canvas();
        shell.paint(&mut canvas);
    }
    surface.present()
}

fn session_surface(info: FramebufferInfo) -> Option<Surface> {
    let width = 320usize;
    let height = 160usize;
    if info.width < 336 || info.height < 596 {
        return None;
    }
    Surface::create(info, 16, 420, width, height, 2, b"GTos Session")
}

/// Shell events use `\\r` for Enter; the raw host keyboard uses `\\n`.
fn translate_host_event(mut event: InputEvent) -> Event {
    if event.kind == gtos_sys::INPUT_KEY_DOWN && event.code == HOST_ENTER {
        event.code = ENTER;
    }
    Event::from_host(event)
}

/// The registered session scheme, serving method calls over kernel pipes.
struct ServiceBus {
    notify_read: u64,
    pending: Vec<u8>,
}

impl ServiceBus {
    fn register() -> Option<Self> {
        let (notify_read, notify_write) = gtos_sys::pipe()?;
        if !gtos_sys::scheme_register(SERVICE_NAME, notify_write) {
            let _ = gtos_sys::close(notify_read);
            let _ = gtos_sys::close(notify_write);
            return None;
        }
        // The registry keeps its own write reference; this process only listens.
        let _ = gtos_sys::close(notify_write);
        Some(Self {
            notify_read,
            pending: Vec::new(),
        })
    }

    /// Takes the next queued `CONNECT <fd> <pid> <url>` notification.
    fn poll(&mut self) -> Option<(u64, Vec<u8>)> {
        let mut chunk = [0u8; 256];
        match gtos_sys::read(self.notify_read, &mut chunk) {
            gtos_sys::ERROR | 0 => {}
            count => {
                let length = usize::try_from(count).unwrap_or(0);
                if length > 0 {
                    self.pending.extend_from_slice(&chunk[..length]);
                }
            }
        }
        while let Some(end) = self.pending.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            if let Some((fd, url)) = parse_connect(trim_eol(&line)) {
                return Some((fd, url.to_vec()));
            }
        }
        None
    }
}

impl Drop for ServiceBus {
    fn drop(&mut self) {
        let _ = gtos_sys::scheme_unregister(SERVICE_NAME);
        let _ = gtos_sys::close(self.notify_read);
    }
}

fn parse_connect(line: &[u8]) -> Option<(u64, &[u8])> {
    let mut parts = line.split(|&byte| byte == b' ');
    if parts.next() != Some(&b"CONNECT"[..]) {
        return None;
    }
    let fd = parse_decimal(parts.next()?)?;
    let _caller = parse_decimal(parts.next()?)?;
    Some((fd, parts.next()?))
}

fn service_method(url: &[u8]) -> Option<&[u8]> {
    url.strip_prefix(SERVICE_PREFIX)
}

fn read_line(fd: u64, out: &mut Vec<u8>) -> bool {
    let deadline = gtos_sys::monotonic_ns().saturating_add(SERVICE_TIMEOUT_NS);
    let mut chunk = [0u8; 64];
    loop {
        match gtos_sys::read(fd, &mut chunk) {
            gtos_sys::ERROR => {}
            0 => return false,
            count => {
                let length = usize::try_from(count).unwrap_or(0);
                if length == 0 || out.len().saturating_add(length) > MAX_REQUEST_BYTES {
                    return false;
                }
                out.extend_from_slice(&chunk[..length]);
                if out.contains(&b'\n') {
                    return true;
                }
            }
        }
        if gtos_sys::monotonic_ns() >= deadline {
            return false;
        }
        gtos_sys::yield_now();
    }
}

fn write_all(fd: u64, bytes: &[u8]) -> bool {
    let mut sent = 0usize;
    while sent < bytes.len() {
        match gtos_sys::write(fd, &bytes[sent..]) {
            gtos_sys::ERROR | 0 => return false,
            count => match usize::try_from(count) {
                Ok(count) if count > 0 && sent + count <= bytes.len() => sent += count,
                _ => return false,
            },
        }
    }
    true
}

fn kcm_config(kcms: &[LauncherRecord], id: &[u8]) -> Option<Vec<u8>> {
    kcms.iter()
        .find(|record| record.id() == id)
        .and_then(|record| read_vfs_file(record.target(), MAX_METADATA_BYTES))
}

fn runner_query(runners: &[LauncherRecord], prefix: &[u8]) -> Vec<u8> {
    let mut response = Vec::new();
    for record in runners
        .iter()
        .filter(|record| record.id().starts_with(prefix) || record.label().starts_with(prefix))
    {
        response.extend_from_slice(record.id());
        response.push(b'\t');
        response.extend_from_slice(record.label());
        response.push(b'\n');
    }
    if response.is_empty() {
        response.extend_from_slice(&b"none\n"[..]);
    }
    response
}

fn runner_launch(runners: &[LauncherRecord], id: &[u8], children: &mut Vec<u64>) -> Vec<u8> {
    let spawned = runners
        .iter()
        .find(|record| record.id() == id)
        .and_then(|record| spawn_target(record.target()));
    match spawned {
        Some(child) => {
            children.push(child);
            b"ok\n"[..].to_vec()
        }
        None => b"fail\n"[..].to_vec(),
    }
}

fn start_service(service: &mut ManagedService) -> bool {
    let Some(pid) = spawn_target(&service.record.target) else {
        return false;
    };
    service.pid = pid;
    service.state = ServiceState::Running;
    let message = alloc::format!(
        "session service {} start {}\r\n",
        ascii(&service.record.id),
        pid
    );
    let _ = gtos_sys::write(1, message.as_bytes());
    true
}

fn start_services(services: &mut [ManagedService]) -> bool {
    for service in services.iter_mut() {
        if !start_service(service) {
            return false;
        }
    }
    write_service_state(services)
}

fn poll_services(services: &mut [ManagedService], allow_restart: bool) -> bool {
    let mut changed = false;
    for service in services.iter_mut() {
        if service.state != ServiceState::Running {
            continue;
        }
        let mut status = 0;
        if gtos_sys::waitpid(service.pid, &mut status) != service.pid {
            continue;
        }
        let id = String::from(ascii(&service.record.id));
        let limit = service.record.restart_limit;
        service.pid = 0;
        service.state = ServiceState::Stopped;
        if allow_restart && service.restarts < limit {
            service.restarts += 1;
            let restarts = service.restarts;
            let message =
                alloc::format!("session service {id} restart {restarts} status {status}\r\n");
            let _ = gtos_sys::write(1, message.as_bytes());
            if !start_service(service) {
                return false;
            }
        } else {
            let restarts = service.restarts;
            let message = alloc::format!(
                "session service {id} stopped status {status} restarts {restarts}\r\n"
            );
            let _ = gtos_sys::write(1, message.as_bytes());
        }
        changed = true;
    }
    if changed {
        write_service_state(services)
    } else {
        true
    }
}

fn service_row(service: &ManagedService) -> Vec<u8> {
    alloc::format!(
        "{}\t{}\t{}\t{}\t{}\n",
        ascii(&service.record.id),
        ascii(&service.record.label),
        service.state.name(),
        service.pid,
        service.restarts
    )
    .into_bytes()
}

fn service_list(services: &[ManagedService]) -> Vec<u8> {
    let mut response = Vec::new();
    for service in services {
        response.extend_from_slice(&service_row(service));
    }
    if response.is_empty() {
        response.extend_from_slice(b"none\n");
    }
    response
}

fn service_status(services: &[ManagedService], id: &[u8]) -> Vec<u8> {
    services
        .iter()
        .find(|service| service.record.id == id)
        .map_or_else(|| b"unknown\n"[..].to_vec(), service_row)
}

fn service_restart(services: &mut [ManagedService], id: &[u8]) -> Vec<u8> {
    let Some(index) = services.iter().position(|service| service.record.id == id) else {
        return b"unknown\n"[..].to_vec();
    };
    if services[index].state == ServiceState::Running {
        return b"running\n"[..].to_vec();
    }
    if services[index].restarts >= services[index].record.restart_limit {
        return b"limit\n"[..].to_vec();
    }
    services[index].restarts += 1;
    if !start_service(&mut services[index]) || !write_service_state(services) {
        return b"fail\n"[..].to_vec();
    }
    b"ok\n"[..].to_vec()
}

fn handle_connection(
    fd: u64,
    url: &[u8],
    runners: &[LauncherRecord],
    kcms: &[LauncherRecord],
    services: &mut [ManagedService],
    children: &mut Vec<u64>,
) -> bool {
    let Some(method) = service_method(url) else {
        return false;
    };
    let mut request = Vec::new();
    if !read_line(fd, &mut request) {
        return false;
    }
    let request = trim_eol(&request);
    let response = match method {
        b"runner/query" => runner_query(runners, request),
        b"runner/launch" => runner_launch(runners, request, children),
        b"kcm/open" => match kcm_config(kcms, request) {
            Some(config) => config,
            None => b"fail\n"[..].to_vec(),
        },
        b"service/list" => service_list(services),
        b"service/status" => service_status(services, request),
        b"service/restart" => service_restart(services, request),
        _ => b"unknown\n"[..].to_vec(),
    };
    write_all(fd, &response)
}

fn pump_service_bus(bus: &mut ServiceBus, state: &mut SessionState<'_>) -> bool {
    let mut ok = true;
    while let Some((fd, url)) = bus.poll() {
        ok &= handle_connection(
            fd,
            &url,
            state.runners,
            state.kcms,
            state.services,
            state.children,
        );
        let _ = gtos_sys::close(fd);
    }
    ok
}

fn call_service(path: &[u8], request: &[u8], expected: &[u8]) -> bool {
    let fd = gtos_sys::open(path);
    if fd == gtos_sys::ERROR {
        return false;
    }
    let mut ok = write_all(fd, request);
    let mut response = Vec::new();
    ok = ok && read_line(fd, &mut response) && response.starts_with(expected);
    let _ = gtos_sys::close(fd);
    ok
}

fn service_client() -> bool {
    call_service(b"/srv/session/runner/query", b"term\n", b"term\tTerminal\n")
        && call_service(b"/srv/session/kcm/open", b"theme\n", b"theme=dark\n")
        && call_service(b"/srv/session/runner/launch", b"term\n", b"ok\n")
        && call_service(
            b"/srv/session/service/list",
            b"\n",
            b"demo\tDemo\tstopped\t0\t2\n",
        )
        && call_service(
            b"/srv/session/service/status",
            b"demo\n",
            b"demo\tDemo\tstopped\t0\t2\n",
        )
        && call_service(b"/srv/session/service/restart", b"demo\n", b"limit\n")
}

fn settle_services(bus: &mut ServiceBus, state: &mut SessionState<'_>) -> bool {
    let deadline = gtos_sys::monotonic_ns().saturating_add(SERVICE_TIMEOUT_NS);
    loop {
        if !pump_service_bus(bus, state) || !poll_services(state.services, true) {
            return false;
        }
        if state
            .services
            .iter()
            .all(|service| service.state == ServiceState::Stopped)
        {
            return true;
        }
        if gtos_sys::monotonic_ns() >= deadline {
            return false;
        }
        gtos_sys::yield_now();
    }
}

fn verify_service_bus(bus: &mut ServiceBus, state: &mut SessionState<'_>) -> bool {
    let child = gtos_sys::fork();
    if child == gtos_sys::ERROR {
        return false;
    }
    if child == 0 {
        let ok = service_client();
        gtos_sys::exit(u64::from(!ok));
    }

    let deadline = gtos_sys::monotonic_ns().saturating_add(SERVICE_TIMEOUT_NS);
    let mut ok = true;
    loop {
        ok &= pump_service_bus(bus, state);
        let mut status = 0;
        if gtos_sys::waitpid(child, &mut status) == child {
            ok &= pump_service_bus(bus, state);
            return ok && status == 0;
        }
        if gtos_sys::monotonic_ns() >= deadline {
            return false;
        }
        gtos_sys::yield_now();
    }
}

/// Applies one batch of shell actions.
///
/// `Some(true)` continues the session, `Some(false)` ends it cleanly and `None`
/// reports a failed action for the caller to log.
fn dispatch_actions(
    actions: Vec<ShellAction>,
    records: &[LauncherRecord],
    kcms: &[LauncherRecord],
    services: &mut [ManagedService],
    children: &mut Vec<u64>,
    surface: &mut Surface,
    shell: &mut DesktopShell<'_>,
) -> Option<bool> {
    for action in actions {
        match action {
            ShellAction::Launch(id) => {
                let child = records
                    .iter()
                    .find(|record| record.id() == id.as_slice())
                    .and_then(|record| spawn_target(record.target()))?;
                children.push(child);
            }
            ShellAction::Applet(id) => {
                if id.as_slice() == b"settings" {
                    let config = kcm_config(kcms, b"theme")?;
                    let _ = gtos_sys::write(1, KCM_OK);
                    let _ = gtos_sys::write(1, b"session kcm theme: ");
                    let _ = gtos_sys::write(1, &config);
                } else {
                    let _ = gtos_sys::write(1, b"session applet action: ");
                    let _ = gtos_sys::write(1, &id);
                    let _ = gtos_sys::write(1, b"\r\n");
                }
            }
            ShellAction::Tray { index, active } => {
                let state = if active { "on" } else { "off" };
                let message = alloc::format!("session tray action: {index} {state}\r\n");
                let _ = gtos_sys::write(1, message.as_bytes());
            }
            ShellAction::Session(request) => {
                let message = alloc::format!("session request: {}\r\n", request.name());
                let _ = gtos_sys::write(1, message.as_bytes());
                reap_children(children);
                if !poll_services(services, false)
                    || !write_state(STATE_STOPPING)
                    || !paint_shell(surface, shell)
                {
                    let _ = gtos_sys::write(1, SHUTDOWN_FAILED);
                    return None;
                }
                let _ = gtos_sys::write(1, SHUTDOWN_OK);
                return Some(false);
            }
        }
    }
    Some(true)
}

type SessionMetadata = (
    Vec<LauncherRecord>,
    Vec<Vec<u8>>,
    Vec<LauncherRecord>,
    Vec<LauncherRecord>,
    Vec<ServiceRecord>,
);

fn load_metadata() -> SessionMetadata {
    let Some(apps_bytes) = read_vfs_file(APPS_PATH, MAX_METADATA_BYTES) else {
        fail(START_FAILED);
    };
    let records = parse_launcher_records(&apps_bytes);
    let Some(auto_bytes) = read_vfs_file(AUTOSTART_PATH, MAX_METADATA_BYTES) else {
        fail(START_FAILED);
    };
    let autostart = parse_autostart(&auto_bytes);
    let Some(runner_bytes) = read_vfs_file(RUNNERS_PATH, MAX_METADATA_BYTES) else {
        fail(START_FAILED);
    };
    let runners = parse_launcher_records(&runner_bytes);
    let Some(kcm_bytes) = read_vfs_file(KCMS_PATH, MAX_METADATA_BYTES) else {
        fail(START_FAILED);
    };
    let kcms = parse_launcher_records(&kcm_bytes);
    let Some(services_bytes) = read_vfs_file(SERVICES_PATH, MAX_METADATA_BYTES) else {
        fail(START_FAILED);
    };
    let services = parse_services(&services_bytes);
    if records.is_empty()
        || autostart.is_empty()
        || runners.is_empty()
        || kcms.is_empty()
        || services.is_empty()
        || !write_state(STATE_STARTING)
    {
        fail(START_FAILED);
    }
    (records, autostart, runners, kcms, services)
}

fn create_shell(records: &[LauncherRecord]) -> DesktopShell<'static> {
    let layout = ShellLayout::new(24, 144, 96, 4, 2, 2);
    let mut shell = DesktopShell::from_records(
        b"Apps",
        records.to_vec(),
        TRAY_ITEMS,
        Theme::dark(),
        Style::new(),
        layout,
    );
    shell.add_applet(b"cpu", b"CPU");
    shell.add_applet(b"settings", b"Settings");
    shell.layout(Rect::new(0, 0, 320, 160));
    shell.focus_tray(1);
    shell
}

fn managed_services(records: Vec<ServiceRecord>) -> Vec<ManagedService> {
    records
        .into_iter()
        .map(|record| ManagedService {
            record,
            state: ServiceState::Stopped,
            pid: 0,
            restarts: 0,
        })
        .collect()
}

fn activate_kcm(
    kcms: &[LauncherRecord],
    services: &mut [ManagedService],
    children: &mut Vec<u64>,
    surface: &mut Surface,
    shell: &mut DesktopShell<'_>,
) -> bool {
    shell.set_shell_focus(ShellFocus::Applet(1));
    if !shell.event(Event::KeyDown {
        code: ENTER,
        modifiers: 0,
    }) {
        return false;
    }
    let actions = shell.take_actions();
    dispatch_actions(actions, &[], kcms, services, children, surface, shell) == Some(true)
        && paint_shell(surface, shell)
}

fn input_loop(
    bus: &mut ServiceBus,
    records: &[LauncherRecord],
    state: &mut SessionState<'_>,
    surface: &mut Surface,
    shell: &mut DesktopShell<'_>,
) {
    loop {
        match gtos_sys::input_event() {
            Ok(InputPoll::Event(raw)) => {
                let event = translate_host_event(raw);
                if shell.event(event) {
                    let actions = shell.take_actions();
                    match dispatch_actions(
                        actions,
                        records,
                        state.kcms,
                        state.services,
                        state.children,
                        surface,
                        shell,
                    ) {
                        Some(true) => {
                            if !paint_shell(surface, shell) {
                                fail(DISPATCH_FAILED);
                            }
                        }
                        Some(false) => return,
                        None => fail(DISPATCH_FAILED),
                    }
                }
            }
            Ok(InputPoll::Idle) => {}
            Err(_) => fail(INPUT_FAILED),
        }
        if !pump_service_bus(bus, state) || !poll_services(state.services, true) {
            fail(DISPATCH_FAILED);
        }
        reap_children(state.children);
        gtos_sys::yield_now();
    }
}

fn main() {
    let _ = gtos_sys::write(1, STARTING);
    let (records, autostart, runners, kcms, service_records) = load_metadata();

    let Some(info) = gtos_sys::framebuffer_info() else {
        fail(START_FAILED);
    };
    let Some(mut surface) = session_surface(info) else {
        fail(START_FAILED);
    };
    let mut shell = create_shell(&records);
    if !paint_shell(&mut surface, &shell) {
        fail(START_FAILED);
    }

    let Some(mut bus) = ServiceBus::register() else {
        fail(START_FAILED);
    };

    let mut children = Vec::new();
    for id in &autostart {
        let Some(child) = records
            .iter()
            .find(|record| record.id() == id.as_slice())
            .and_then(|record| spawn_target(record.target()))
        else {
            fail(AUTOSTART_FAILED);
        };
        children.push(child);
    }
    reap_children(&mut children);
    if !write_state(STATE_RUNNING) {
        fail(AUTOSTART_FAILED);
    }
    let _ = gtos_sys::write(1, AUTOSTART_OK);

    let mut services = managed_services(service_records);
    let mut state = SessionState {
        runners: &runners,
        kcms: &kcms,
        services: &mut services,
        children: &mut children,
    };
    if !start_services(state.services) || !settle_services(&mut bus, &mut state) {
        fail(SERVICES_FAILED);
    }
    let _ = gtos_sys::write(1, SERVICES_OK);

    if !verify_service_bus(&mut bus, &mut state) {
        fail(BUS_FAILED);
    }
    let _ = gtos_sys::write(1, BUS_OK);

    // A synthetic activation exercises the KCM applet path without consuming a
    // host keystroke that the shell-driven input test also observes.
    if !activate_kcm(
        state.kcms,
        state.services,
        state.children,
        &mut surface,
        &mut shell,
    ) {
        fail(KCM_FAILED);
    }
    shell.focus_tray(1);
    let _ = gtos_sys::write(1, WAITING);

    input_loop(&mut bus, &records, &mut state, &mut surface, &mut shell);
}

//! A small IPv4 network stack.
//!
//! Implements the RTL8139 NIC (the adapter QEMU attaches for
//! `-nic user,model=rtl8139`), Ethernet, ARP, IPv4, ICMP, UDP, a DNS resolver
//! and a synchronous TCP client, which together are enough to `ping`, resolve
//! host names and fetch web pages over HTTP.
//!
//! The driver is polled from the shell rather than interrupt-driven, so the
//! whole stack stays single-threaded. DMA buffers are carved out of the kernel
//! heap, whose addresses live in the bootloader's physical-memory window, so
//! the physical address the NIC needs is simply
//! `virtual_address - physical_memory_offset`.

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use core::sync::atomic::{AtomicU32, Ordering};

use spin::Mutex;
use x86_64::instructions::port::Port;

use crate::{interrupts, pci};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// MAC address programmed into the adapter (QEMU's default guest NIC).
pub const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

/// The runtime IPv4 configuration.
#[derive(Clone, Copy)]
pub struct Address {
    /// Our own address.
    pub ip: [u8; 4],
    /// The subnet mask.
    pub netmask: [u8; 4],
    /// The default gateway.
    pub gateway: [u8; 4],
    /// The DNS server to query.
    pub dns: [u8; 4],
    /// True once a DHCP lease has been accepted.
    pub dhcp: bool,
}

/// The address configuration in use.
///
/// It starts at QEMU's user-mode ("SLIRP") defaults so the stack works even
/// without a DHCP server, and [`dhcp`] replaces it with whatever a server hands
/// out. SLIRP itself answers DHCP, so a normal boot ends up on the same
/// addresses through the real protocol.
static ADDRESS: Mutex<Address> = Mutex::new(Address {
    ip: [10, 0, 2, 15],
    netmask: [255, 255, 255, 0],
    gateway: [10, 0, 2, 2],
    dns: [10, 0, 2, 3],
    dhcp: false,
});

/// The current address configuration.
pub fn address() -> Address {
    *ADDRESS.lock()
}

/// The current IPv4 address.
pub fn ip() -> [u8; 4] {
    ADDRESS.lock().ip
}

/// The current default gateway.
pub fn gateway() -> [u8; 4] {
    ADDRESS.lock().gateway
}

/// The current DNS server.
pub fn dns() -> [u8; 4] {
    ADDRESS.lock().dns
}

// ---------------------------------------------------------------------------
// RTL8139 registers
// ---------------------------------------------------------------------------

const VENDOR: u16 = 0x10EC;
const DEVICE: u16 = 0x8139;

const REG_MAC: u16 = 0x00;
const REG_TSD: u16 = 0x10; // + 4 * descriptor
const REG_TSAD: u16 = 0x20; // + 4 * descriptor
const REG_RBSTART: u16 = 0x30;
const REG_CMD: u16 = 0x37;
const REG_CAPR: u16 = 0x38;
const REG_IMR: u16 = 0x3C;
const REG_ISR: u16 = 0x3E;
const REG_TCR: u16 = 0x40;
const REG_RCR: u16 = 0x44;
const REG_CONFIG1: u16 = 0x52;

const CMD_RESET: u8 = 0x10;
const CMD_RX_ENABLE: u8 = 0x08;
const CMD_TX_ENABLE: u8 = 0x04;
const CMD_RX_EMPTY: u8 = 0x01;

/// Accept every frame: broadcast, multicast, our address and promiscuous.
const RCR_ACCEPT_ALL: u32 = 0x0000_000F;
/// Do not wrap packets inside the ring, so a frame near the end stays
/// contiguous in the slack we allocated past the ring.
const RCR_NO_WRAP: u32 = 0x0000_0080;

const RX_RING: usize = 8192;
const RX_SLACK: usize = 16 + 1536;
const RX_TOTAL: usize = RX_RING + RX_SLACK;
const TX_COUNT: usize = 4;
const TX_BUF: usize = 2048;
const DMA_BYTES: usize = RX_TOTAL + TX_COUNT * TX_BUF;

/// The largest frame we will ever hand back to the protocol layer.
const FRAME_BYTES: usize = 2048;

/// Timeout for ARP, in PIT ticks (10 ms each).
const ARP_TIMEOUT: u64 = 100;
/// Timeout for an ICMP echo reply.
const PING_TIMEOUT: u64 = 200;
/// Timeout for a DNS answer.
const DNS_TIMEOUT: u64 = 300;
/// Timeout for TCP segments.
const TCP_TIMEOUT: u64 = 500;

struct Nic {
    io: u16,
    irq: u8,
    /// Virtual-to-physical offset for the DMA region.
    offset: u64,
    /// One leaked region: the RX ring followed by the four TX buffers.
    dma: &'static mut [u8],
    rx_read: usize,
    tx_next: usize,
}

static NIC: Mutex<Option<Nic>> = Mutex::new(None);

/// A resolved ARP entry.
static ARP_CACHE: Mutex<Vec<([u8; 4], [u8; 6])>> = Mutex::new(Vec::new());

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Probes for the RTL8139 and brings it up.
///
/// Returns `false` when no supported adapter is present. `physical_memory_offset`
/// is `BootInfo::physical_memory_offset`, used to turn the heap addresses of the
/// DMA buffers into the physical addresses the NIC needs.
pub fn init(physical_memory_offset: u64) -> bool {
    let Some(device) = pci::find(VENDOR, DEVICE) else {
        return false;
    };
    pci::enable(&device);

    let io = (device.bar0 & 0xFFFC) as u16;
    if io == 0 {
        return false;
    }

    let dma = alloc_dma(DMA_BYTES);

    // SAFETY: `io` is the adapter's I/O BAR; the DMA region is leaked for the
    // lifetime of the kernel and stays at a fixed physical address.
    unsafe {
        // Power the chip on.
        Port::<u8>::new(io + REG_CONFIG1).write(0x00);

        // Soft reset and wait for the RST bit to clear.
        let mut command = Port::<u8>::new(io + REG_CMD);
        command.write(CMD_RESET);
        for _ in 0..100_000 {
            if command.read() & CMD_RESET == 0 {
                break;
            }
        }

        // Program our MAC address.
        for (index, byte) in MAC.iter().enumerate() {
            Port::<u8>::new(io + REG_MAC + index as u16).write(*byte);
        }

        // Point the chip at the receive ring.
        Port::<u32>::new(io + REG_RBSTART).write(physical(dma.as_ptr(), physical_memory_offset));

        // Receive configuration.
        Port::<u32>::new(io + REG_RCR).write(RCR_ACCEPT_ALL | RCR_NO_WRAP);
        // A sane transmit configuration (default inter-frame gap).
        Port::<u32>::new(io + REG_TCR).write(0x0000_0300);

        // Polled: no interrupts, clear anything pending.
        Port::<u16>::new(io + REG_IMR).write(0x0000);
        Port::<u16>::new(io + REG_ISR).write(0xFFFF);

        // The chip expects CAPR to trail the read pointer by 16 bytes.
        Port::<u16>::new(io + REG_CAPR).write(0u16.wrapping_sub(16));

        // Enable the transmitter and receiver.
        command.write(CMD_TX_ENABLE | CMD_RX_ENABLE);
    }

    *NIC.lock() = Some(Nic {
        io,
        irq: device.irq,
        offset: physical_memory_offset,
        dma,
        rx_read: 0,
        tx_next: 0,
    });
    true
}

/// Whether an adapter was found and brought up.
pub fn is_up() -> bool {
    NIC.lock().is_some()
}

/// `(I/O base, IRQ line)` of the adapter, if one is up.
pub fn adapter() -> Option<(u16, u8)> {
    NIC.lock().as_ref().map(|nic| (nic.io, nic.irq))
}

/// Leaks a zeroed, 16-byte-aligned DMA region from the kernel heap.
fn alloc_dma(size: usize) -> &'static mut [u8] {
    use alloc::alloc::{Layout, alloc_zeroed};

    let layout = Layout::from_size_align(size, 16).expect("valid DMA layout");
    // SAFETY: the layout has a non-zero size and the allocation is never freed.
    let pointer = unsafe { alloc_zeroed(layout) };
    assert!(!pointer.is_null(), "network DMA allocation failed");
    // SAFETY: `pointer` is valid for `size` bytes and leaked for good.
    unsafe { core::slice::from_raw_parts_mut(pointer, size) }
}

/// Converts a heap virtual address into the physical address for DMA.
fn physical(virtual_address: *const u8, offset: u64) -> u32 {
    (virtual_address as u64 - offset) as u32
}

/// Reads one received frame into `out`, returning its length.
fn receive(out: &mut [u8]) -> Option<usize> {
    let mut guard = NIC.lock();
    let nic = guard.as_mut()?;
    let io = nic.io;

    // SAFETY: `io` is the adapter's I/O BAR.
    let empty = unsafe { Port::<u8>::new(io + REG_CMD).read() } & CMD_RX_EMPTY != 0;
    if empty {
        return None;
    }

    let offset = nic.rx_read;
    let status = u16::from_le_bytes([nic.dma[offset], nic.dma[offset + 1]]);
    let length = usize::from(u16::from_le_bytes([
        nic.dma[offset + 2],
        nic.dma[offset + 3],
    ]));
    // The length field counts the trailing CRC, so the payload is 4 bytes less.
    if status & 0x0001 == 0 || length < 4 || offset + length > RX_TOTAL {
        return None;
    }

    let payload = &nic.dma[offset + 4..offset + length];
    let copied = payload.len().min(out.len());
    out[..copied].copy_from_slice(&payload[..copied]);

    // Advance the ring read pointer, keeping it 4-byte aligned.
    nic.rx_read = ((offset + length + 4 + 3) & !3) % RX_RING;

    // SAFETY: as above.
    unsafe {
        Port::<u16>::new(io + REG_CAPR).write((nic.rx_read as u16).wrapping_sub(16));
        Port::<u16>::new(io + REG_ISR).write(0x0001);
    }
    Some(copied)
}

/// Transmits one Ethernet frame.
fn transmit(frame: &[u8]) -> bool {
    let mut guard = NIC.lock();
    let Some(nic) = guard.as_mut() else {
        return false;
    };
    if frame.is_empty() || frame.len() > TX_BUF {
        return false;
    }

    // QEMU always transmits from its own round-robin descriptor, so the
    // descriptors must be used in order 0, 1, 2, 3, 0, ...
    let index = nic.tx_next % TX_COUNT;
    let base = RX_TOTAL + index * TX_BUF;
    nic.dma[base..base + frame.len()].copy_from_slice(frame);
    let address = physical(nic.dma[base..].as_ptr(), nic.offset);
    let io = nic.io;

    // SAFETY: `io` is the adapter's I/O BAR and the buffer is a leaked DMA region.
    let sent = unsafe {
        Port::<u32>::new(io + REG_TSAD + index as u16 * 4).write(address);
        let mut status = Port::<u32>::new(io + REG_TSD + index as u16 * 4);
        // Writing the length clears the chip's OWN bit and starts the transfer.
        status.write(frame.len() as u32);
        let mut sent = false;
        for _ in 0..1_000_000 {
            let value = status.read();
            if value & 0x0000_8000 != 0 {
                sent = true; // transmit OK
                break;
            }
            if value & 0x0000_4000 != 0 {
                break; // transmit underrun / aborted
            }
        }
        Port::<u16>::new(io + REG_ISR).write(0x0004);
        sent
    };

    // The chip transmits from its own round-robin descriptor on every TSD write,
    // so the host pointer advances whether or not the status poll saw TOK.
    nic.tx_next += 1;
    sent
}

// ---------------------------------------------------------------------------
// Frame helpers
// ---------------------------------------------------------------------------

fn send_frame(destination: [u8; 6], ethertype: u16, payload: &[u8]) -> bool {
    let mut frame = Vec::with_capacity(14 + payload.len());
    frame.extend_from_slice(&destination);
    frame.extend_from_slice(&MAC);
    frame.extend_from_slice(&ethertype.to_be_bytes());
    frame.extend_from_slice(payload);
    // Pad short frames to the 60-byte Ethernet minimum.
    if frame.len() < 60 {
        frame.resize(60, 0);
    }
    transmit(&frame)
}

fn frame_ethertype(frame: &[u8]) -> Option<u16> {
    if frame.len() < 14 {
        return None;
    }
    Some(u16::from_be_bytes([frame[12], frame[13]]))
}

/// Waits until `matches` accepts a received frame, or the timeout expires.
///
/// The timeout is measured in PIT ticks. Frames that do not match are dropped.
fn wait_for<F>(mut matches: F, timeout_ticks: u64) -> Option<Vec<u8>>
where
    F: FnMut(&[u8]) -> bool,
{
    let start = interrupts::ticks();
    let mut buffer = [0u8; FRAME_BYTES];
    let mut spins = 0u64;
    loop {
        if let Some(length) = receive(&mut buffer) {
            if matches(&buffer[..length]) {
                return Some(buffer[..length].to_vec());
            }
        }
        if interrupts::ticks().wrapping_sub(start) >= timeout_ticks || spins > 500_000_000 {
            return None;
        }
        spins += 1;
        if x86_64::instructions::interrupts::are_enabled() {
            x86_64::instructions::hlt();
        } else {
            core::hint::spin_loop();
        }
    }
}

// ---------------------------------------------------------------------------
// Checksums
// ---------------------------------------------------------------------------

/// Folds a 32-bit accumulator into the one's-complement 16-bit checksum.
fn finish_checksum(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// One's-complement sum of `data` interpreted as big-endian 16-bit words.
fn ones_complement_sum(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    let mut chunks = data.chunks_exact(2);
    for chunk in &mut chunks {
        sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
    }
    if let [last] = chunks.remainder() {
        sum += u32::from(*last) << 8;
    }
    sum
}

fn ip_checksum(header: &[u8]) -> u16 {
    finish_checksum(ones_complement_sum(header))
}

fn tcp_checksum(source: [u8; 4], destination: [u8; 4], segment: &[u8]) -> u16 {
    let mut sum = ones_complement_sum(segment);
    sum += u32::from(u16::from_be_bytes([source[0], source[1]]));
    sum += u32::from(u16::from_be_bytes([source[2], source[3]]));
    sum += u32::from(u16::from_be_bytes([destination[0], destination[1]]));
    sum += u32::from(u16::from_be_bytes([destination[2], destination[3]]));
    sum += 6; // TCP protocol number for the pseudo-header.
    sum += segment.len() as u32;
    finish_checksum(sum)
}

/// The UDP checksum over the datagram and its IPv4 pseudo-header.
///
/// A computed value of zero is transmitted as 0xFFFF, since zero means "no
/// checksum" in UDP.
fn udp_checksum(source: [u8; 4], destination: [u8; 4], datagram: &[u8]) -> u16 {
    let mut sum = ones_complement_sum(datagram);
    sum += u32::from(u16::from_be_bytes([source[0], source[1]]));
    sum += u32::from(u16::from_be_bytes([source[2], source[3]]));
    sum += u32::from(u16::from_be_bytes([destination[0], destination[1]]));
    sum += u32::from(u16::from_be_bytes([destination[2], destination[3]]));
    sum += 17; // UDP protocol number for the pseudo-header.
    sum += datagram.len() as u32;
    match finish_checksum(sum) {
        0 => 0xFFFF,
        checksum => checksum,
    }
}

// ---------------------------------------------------------------------------
// ARP
// ---------------------------------------------------------------------------

fn lookup_arp(address: [u8; 4]) -> Option<[u8; 6]> {
    ARP_CACHE
        .lock()
        .iter()
        .find(|entry| entry.0 == address)
        .map(|entry| entry.1)
}

fn remember_arp(address: [u8; 4], mac: [u8; 6]) {
    let mut cache = ARP_CACHE.lock();
    if let Some(entry) = cache.iter_mut().find(|entry| entry.0 == address) {
        entry.1 = mac;
    } else {
        cache.push((address, mac));
    }
}

/// Parses an ARP reply, returning the sender's address and MAC.
fn parse_arp_reply(frame: &[u8]) -> Option<([u8; 4], [u8; 6])> {
    if frame_ethertype(frame) != Some(0x0806) || frame.len() < 14 + 28 {
        return None;
    }
    let payload = &frame[14..];
    if u16::from_be_bytes([payload[6], payload[7]]) != 2 {
        return None; // not a reply
    }
    let mut address = [0u8; 4];
    address.copy_from_slice(&payload[14..18]);
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&payload[8..14]);
    Some((address, mac))
}

/// Resolves `address` to a MAC, asking the network if it is not cached.
fn resolve_mac(address: [u8; 4]) -> Option<[u8; 6]> {
    if address == [255, 255, 255, 255] {
        return Some([0xFF; 6]);
    }
    if let Some(mac) = lookup_arp(address) {
        return Some(mac);
    }

    let mut request = Vec::with_capacity(28);
    request.extend_from_slice(&1u16.to_be_bytes()); // hardware type: Ethernet
    request.extend_from_slice(&0x0800u16.to_be_bytes()); // protocol: IPv4
    request.push(6); // hardware length
    request.push(4); // protocol length
    request.extend_from_slice(&1u16.to_be_bytes()); // opcode: request
    request.extend_from_slice(&MAC);
    request.extend_from_slice(&ip());
    request.extend_from_slice(&[0u8; 6]);
    request.extend_from_slice(&address);
    if !send_frame([0xFF; 6], 0x0806, &request) {
        return None;
    }

    let reply = wait_for(
        |frame| parse_arp_reply(frame).is_some_and(|(sender, _)| sender == address),
        ARP_TIMEOUT,
    )?;
    let (_, mac) = parse_arp_reply(&reply)?;
    remember_arp(address, mac);
    Some(mac)
}

// ---------------------------------------------------------------------------
// IPv4
// ---------------------------------------------------------------------------

struct IpPacket<'a> {
    payload: &'a [u8],
    protocol: u8,
    source: [u8; 4],
}

fn parse_ipv4(frame: &[u8]) -> Option<IpPacket<'_>> {
    if frame_ethertype(frame) != Some(0x0800) || frame.len() < 14 + 20 {
        return None;
    }
    let packet = &frame[14..];
    let header_length = usize::from(packet[0] & 0x0F) * 4;
    let total_length = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    if header_length < 20 || total_length < header_length || packet.len() < total_length {
        return None;
    }
    let mut source = [0u8; 4];
    source.copy_from_slice(&packet[12..16]);
    Some(IpPacket {
        payload: &packet[header_length..total_length],
        protocol: packet[9],
        source,
    })
}

/// The next hop for `destination`: the host itself when it is on the local
/// subnet, otherwise the default gateway. QEMU's SLIRP only answers ARP for
/// the addresses it owns, so off-subnet traffic must be sent to the gateway's
/// MAC while keeping the real destination in the IP header.
fn next_hop(destination: [u8; 4]) -> [u8; 4] {
    let config = address();
    let local =
        (0..4).all(|i| destination[i] & config.netmask[i] == config.ip[i] & config.netmask[i]);
    if local { destination } else { config.gateway }
}

/// Builds and sends one IPv4 packet to an explicit next-hop MAC.
fn send_ipv4_to(
    source: [u8; 4],
    destination: [u8; 4],
    destination_mac: [u8; 6],
    protocol: u8,
    payload: &[u8],
) -> bool {
    let total_length = 20 + payload.len();
    let mut packet = Vec::with_capacity(total_length);
    packet.push(0x45); // IPv4, 5-word header
    packet.push(0x00); // DSCP / ECN
    packet.extend_from_slice(&(total_length as u16).to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes()); // identification
    packet.extend_from_slice(&0x4000u16.to_be_bytes()); // don't fragment
    packet.push(64); // TTL
    packet.push(protocol);
    packet.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    packet.extend_from_slice(&source);
    packet.extend_from_slice(&destination);
    let checksum = ip_checksum(&packet);
    packet[10..12].copy_from_slice(&checksum.to_be_bytes());
    packet.extend_from_slice(payload);
    send_frame(destination_mac, 0x0800, &packet)
}

fn send_ipv4(destination: [u8; 4], protocol: u8, payload: &[u8]) -> bool {
    let Some(destination_mac) = resolve_mac(next_hop(destination)) else {
        return false;
    };
    send_ipv4_to(ip(), destination, destination_mac, protocol, payload)
}

// ---------------------------------------------------------------------------
// ICMP
// ---------------------------------------------------------------------------

/// Sends an ICMP echo request and returns the round-trip time in milliseconds.
pub fn ping(destination: [u8; 4]) -> Option<u64> {
    let mut echo = Vec::with_capacity(40);
    echo.push(8); // echo request
    echo.push(0); // code
    echo.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    echo.extend_from_slice(&0x1234u16.to_be_bytes()); // identifier
    echo.extend_from_slice(&1u16.to_be_bytes()); // sequence
    for index in 0..32u8 {
        echo.push(index);
    }
    let checksum = ip_checksum(&echo);
    echo[2..4].copy_from_slice(&checksum.to_be_bytes());

    let start = interrupts::ticks();
    if !send_ipv4(destination, 1, &echo) {
        return None;
    }
    wait_for(
        |frame| {
            parse_ipv4(frame).is_some_and(|packet| {
                packet.protocol == 1
                    && packet.source == destination
                    && packet.payload.len() >= 8
                    && packet.payload[0] == 0 // echo reply
            })
        },
        PING_TIMEOUT,
    )?;
    let elapsed = interrupts::ticks().wrapping_sub(start);
    Some(elapsed * (1000 / u64::from(interrupts::TICK_HZ)))
}

// ---------------------------------------------------------------------------
// UDP + DNS
// ---------------------------------------------------------------------------

fn send_udp(destination: [u8; 4], source_port: u16, destination_port: u16, payload: &[u8]) -> bool {
    let mut datagram = Vec::with_capacity(8 + payload.len());
    datagram.extend_from_slice(&source_port.to_be_bytes());
    datagram.extend_from_slice(&destination_port.to_be_bytes());
    datagram.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    datagram.extend_from_slice(&0u16.to_be_bytes()); // checksum optional for IPv4
    datagram.extend_from_slice(payload);
    send_ipv4(destination, 17, &datagram)
}

/// Parses a UDP datagram, returning `(payload, source port, destination port)`.
fn parse_udp<'a>(packet: &IpPacket<'a>) -> Option<(&'a [u8], u16, u16)> {
    let datagram = packet.payload;
    if datagram.len() < 8 {
        return None;
    }
    let source_port = u16::from_be_bytes([datagram[0], datagram[1]]);
    let destination_port = u16::from_be_bytes([datagram[2], datagram[3]]);
    let length = usize::from(u16::from_be_bytes([datagram[4], datagram[5]]));
    let end = length.clamp(8, datagram.len());
    Some((&datagram[8..end], source_port, destination_port))
}

const DNS_PORT: u16 = 53;
const DNS_LOCAL_PORT: u16 = 53_000;
const DNS_ID: u16 = 0x1234;

fn build_dns_query(name: &str) -> Vec<u8> {
    let mut query = Vec::new();
    query.extend_from_slice(&DNS_ID.to_be_bytes());
    query.extend_from_slice(&0x0100u16.to_be_bytes()); // standard query, recursion desired
    query.extend_from_slice(&1u16.to_be_bytes()); // questions
    query.extend_from_slice(&0u16.to_be_bytes()); // answer RRs
    query.extend_from_slice(&0u16.to_be_bytes()); // authority RRs
    query.extend_from_slice(&0u16.to_be_bytes()); // additional RRs
    for label in name.split('.').filter(|label| !label.is_empty()) {
        query.push(label.len() as u8);
        query.extend_from_slice(label.as_bytes());
    }
    query.push(0); // root label
    query.extend_from_slice(&1u16.to_be_bytes()); // type A
    query.extend_from_slice(&1u16.to_be_bytes()); // class IN
    query
}

/// Steps over a (possibly compressed) DNS name, returning the next offset.
fn skip_dns_name(message: &[u8], mut position: usize) -> Option<usize> {
    loop {
        let length = *message.get(position)?;
        if length == 0 {
            return Some(position + 1);
        }
        if length & 0xC0 == 0xC0 {
            return Some(position + 2); // compression pointer
        }
        position += 1 + usize::from(length);
    }
}

fn parse_dns_answer(message: &[u8]) -> Option<[u8; 4]> {
    if message.len() < 12 {
        return None;
    }
    let questions = usize::from(u16::from_be_bytes([message[4], message[5]]));
    let answers = usize::from(u16::from_be_bytes([message[6], message[7]]));
    let mut position = 12;
    for _ in 0..questions {
        position = skip_dns_name(message, position)? + 4;
        if position > message.len() {
            return None;
        }
    }
    for _ in 0..answers {
        position = skip_dns_name(message, position)?;
        if position + 10 > message.len() {
            return None;
        }
        let kind = u16::from_be_bytes([message[position], message[position + 1]]);
        let length = usize::from(u16::from_be_bytes([
            message[position + 8],
            message[position + 9],
        ]));
        position += 10;
        if position + length > message.len() {
            return None;
        }
        if kind == 1 && length == 4 {
            let mut address = [0u8; 4];
            address.copy_from_slice(&message[position..position + 4]);
            return Some(address);
        }
        position += length;
    }
    None
}

/// Resolves a host name through SLIRP's DNS proxy.
pub fn resolve(name: &str) -> Option<[u8; 4]> {
    let server = dns();
    let query = build_dns_query(name);
    if !send_udp(server, DNS_LOCAL_PORT, DNS_PORT, &query) {
        return None;
    }
    let frame = wait_for(
        |frame| {
            parse_ipv4(frame).is_some_and(|packet| {
                packet.protocol == 17
                    && packet.source == server
                    && parse_udp(&packet).is_some_and(|(_, source, destination)| {
                        destination == DNS_LOCAL_PORT && source == DNS_PORT
                    })
            })
        },
        DNS_TIMEOUT,
    )?;
    let packet = parse_ipv4(&frame)?;
    let (message, _, _) = parse_udp(&packet)?;
    parse_dns_answer(message)
}

// ---------------------------------------------------------------------------
// DHCP
// ---------------------------------------------------------------------------

const DHCP_CLIENT_PORT: u16 = 68;
const DHCP_SERVER_PORT: u16 = 67;
/// The BOOTP fixed header (236 bytes) plus the magic cookie.
const DHCP_FIXED: usize = 240;
const DHCP_MAGIC: [u8; 4] = [99, 130, 83, 99];

const DHCP_DISCOVER: u8 = 1;
const DHCP_OFFER: u8 = 2;
const DHCP_REQUEST: u8 = 3;
const DHCP_ACK: u8 = 5;

/// Timeout for a DHCP reply.
const DHCP_TIMEOUT: u64 = 500;

/// The transaction id of the current exchange.
static DHCP_XID: AtomicU32 = AtomicU32::new(0x3903_f326);

/// A decoded DHCP reply (OFFER or ACK).
struct DhcpReply {
    message_type: u8,
    /// The address the server offers (`yiaddr`).
    address: [u8; 4],
    /// The server identifier (option 54).
    server: [u8; 4],
    netmask: Option<[u8; 4]>,
    gateway: Option<[u8; 4]>,
    dns: Option<[u8; 4]>,
}

/// Builds a BOOTP/DHCP request message.
fn build_dhcp(message_type: u8, requested: Option<[u8; 4]>, server: Option<[u8; 4]>) -> Vec<u8> {
    let mut message = alloc::vec![0u8; DHCP_FIXED];
    message[0] = 1; // op: BOOTREQUEST
    message[1] = 1; // htype: Ethernet
    message[2] = 6; // hlen
    message[3] = 0; // hops
    message[4..8].copy_from_slice(&DHCP_XID.load(Ordering::Relaxed).to_be_bytes());
    // secs, flags, ciaddr, yiaddr, siaddr and giaddr stay zero.
    message[28..34].copy_from_slice(&MAC);
    message[236..240].copy_from_slice(&DHCP_MAGIC);

    let mut options: Vec<u8> = Vec::new();
    options.push(53); // DHCP message type
    options.push(1);
    options.push(message_type);
    if message_type == DHCP_DISCOVER {
        options.push(55); // parameter request list
        options.push(4);
        options.extend_from_slice(&[1, 3, 6, 54]); // mask, router, DNS, server id
    }
    if let Some(address) = requested {
        options.push(50); // requested IP address
        options.push(4);
        options.extend_from_slice(&address);
    }
    if let Some(server) = server {
        options.push(54); // server identifier
        options.push(4);
        options.extend_from_slice(&server);
    }
    options.push(255); // end
    message.extend_from_slice(&options);

    // BOOTP requires at least 300 bytes; pad the tail.
    if message.len() < 300 {
        message.resize(300, 0);
    }
    message
}

/// Sends a DHCP message as an IPv4 broadcast from 0.0.0.0.
fn send_dhcp(payload: &[u8]) -> bool {
    let source = [0, 0, 0, 0];
    let destination = [255, 255, 255, 255];
    let mut datagram = Vec::with_capacity(8 + payload.len());
    datagram.extend_from_slice(&DHCP_CLIENT_PORT.to_be_bytes());
    datagram.extend_from_slice(&DHCP_SERVER_PORT.to_be_bytes());
    datagram.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    datagram.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    datagram.extend_from_slice(payload);
    let checksum = udp_checksum(source, destination, &datagram);
    datagram[6..8].copy_from_slice(&checksum.to_be_bytes());
    send_ipv4_to(source, destination, [0xFF; 6], 17, &datagram)
}

/// Decodes a DHCP message carried in an Ethernet frame.
fn dhcp_message(frame: &[u8]) -> Option<DhcpReply> {
    let packet = parse_ipv4(frame)?;
    if packet.protocol != 17 {
        return None;
    }
    let (payload, source_port, destination_port) = parse_udp(&packet)?;
    if source_port != DHCP_SERVER_PORT || destination_port != DHCP_CLIENT_PORT {
        return None;
    }
    parse_dhcp(payload)
}

fn parse_dhcp(message: &[u8]) -> Option<DhcpReply> {
    if message.len() < DHCP_FIXED || message[0] != 2 {
        return None; // not a BOOTREPLY
    }
    if message[4..8] != DHCP_XID.load(Ordering::Relaxed).to_be_bytes() {
        return None; // a different transaction
    }
    if message[236..240] != DHCP_MAGIC {
        return None;
    }

    let mut address = [0u8; 4];
    address.copy_from_slice(&message[16..20]); // yiaddr
    let mut reply = DhcpReply {
        message_type: 0,
        address,
        server: [0; 4],
        netmask: None,
        gateway: None,
        dns: None,
    };

    let mut position = DHCP_FIXED;
    while position < message.len() {
        let code = message[position];
        if code == 255 {
            break; // end
        }
        if code == 0 {
            position += 1; // pad
            continue;
        }
        if position + 1 >= message.len() {
            break;
        }
        let length = usize::from(message[position + 1]);
        let start = position + 2;
        if start + length > message.len() {
            break;
        }
        let value = &message[start..start + length];
        match (code, length) {
            (53, 1) => reply.message_type = value[0],
            (54, 4..) => reply.server.copy_from_slice(&value[..4]),
            (1, 4..) => reply.netmask = Some(four(value)),
            (3, 4..) => reply.gateway = Some(four(value)),
            (6, 4..) => reply.dns = Some(four(value)),
            _ => {}
        }
        position = start + length;
    }
    Some(reply)
}

/// Copies the first four bytes of `value` into an address.
fn four(value: &[u8]) -> [u8; 4] {
    let mut address = [0u8; 4];
    address.copy_from_slice(&value[..4]);
    address
}

/// Runs the DHCP client: DISCOVER, wait for OFFER, REQUEST, wait for ACK.
///
/// On success the runtime address configuration is replaced with the lease and
/// `true` is returned; on any failure the existing configuration is untouched.
pub fn dhcp() -> bool {
    // A fresh transaction id, so a stale reply cannot be mistaken for ours.
    DHCP_XID.store(
        0x3903_0000 | (interrupts::ticks() as u32 & 0xFFFF),
        Ordering::Relaxed,
    );

    if !send_dhcp(&build_dhcp(DHCP_DISCOVER, None, None)) {
        return false;
    }
    let Some(offer_frame) = wait_for(
        |frame| dhcp_message(frame).is_some_and(|reply| reply.message_type == DHCP_OFFER),
        DHCP_TIMEOUT,
    ) else {
        return false;
    };
    let Some(offer) = dhcp_message(&offer_frame) else {
        return false;
    };

    let requested = offer.address;
    let server = if offer.server == [0, 0, 0, 0] {
        offer.address
    } else {
        offer.server
    };
    if !send_dhcp(&build_dhcp(DHCP_REQUEST, Some(requested), Some(server))) {
        return false;
    }
    let Some(ack_frame) = wait_for(
        |frame| {
            dhcp_message(frame)
                .is_some_and(|reply| reply.message_type == DHCP_ACK && reply.address == requested)
        },
        DHCP_TIMEOUT,
    ) else {
        return false;
    };
    let Some(ack) = dhcp_message(&ack_frame) else {
        return false;
    };

    let mut config = ADDRESS.lock();
    config.ip = ack.address;
    config.netmask = ack.netmask.unwrap_or([255, 255, 255, 0]);
    config.gateway = ack.gateway.or(offer.gateway).unwrap_or(config.gateway);
    config.dns = ack.dns.or(offer.dns).unwrap_or(config.dns);
    config.dhcp = true;
    drop(config);

    // The cached MACs belong to the previous address; they may be stale now.
    ARP_CACHE.lock().clear();
    true
}

// ---------------------------------------------------------------------------
// TCP, TLS, URL and HTTP
// ---------------------------------------------------------------------------

use embedded_io::{ErrorKind, Write as _};
use embedded_tls::blocking::TlsConnection;
use embedded_tls::{Aes128GcmSha256, TlsConfig, TlsContext, UnsecureProvider};
use rand_core::{CryptoRng, RngCore};

const TCP_FIN: u8 = 0x01;
const TCP_SYN: u8 = 0x02;
const TCP_RST: u8 = 0x04;
const TCP_PSH: u8 = 0x08;
const TCP_ACK: u8 = 0x10;

/// Upper bound on a fetched document. The kernel heap is large enough for a
/// real page, but an unlimited response would let one hostile server exhaust it.
const RESPONSE_LIMIT: usize = 1024 * 1024;
/// Maximum number of 3xx redirects to follow for one navigation.
const MAX_REDIRECTS: usize = 5;

struct Segment {
    sequence: u32,
    flags: u8,
    payload: Vec<u8>,
}

/// A single synchronous TCP connection.
struct Connection {
    local_port: u16,
    remote_ip: [u8; 4],
    remote_port: u16,
    sequence: u32,
    acknowledgement: u32,
}

impl Connection {
    /// Opens a connection with the three-way handshake.
    fn connect(remote_ip: [u8; 4], remote_port: u16) -> Option<Self> {
        let mut connection = Self {
            local_port: 40_000 + (interrupts::ticks() as u16 % 1000),
            remote_ip,
            remote_port,
            sequence: 0x1000_0000,
            acknowledgement: 0,
        };

        connection.send(TCP_SYN, &[]);
        let syn_ack = connection.wait_segment()?;
        if (syn_ack.flags & (TCP_SYN | TCP_ACK)) != (TCP_SYN | TCP_ACK) {
            return None;
        }
        connection.acknowledgement = syn_ack.sequence.wrapping_add(1);
        connection.sequence = connection.sequence.wrapping_add(1);
        connection.send(TCP_ACK, &[]);
        Some(connection)
    }

    fn build(&self, flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut segment = Vec::with_capacity(20 + payload.len());
        segment.extend_from_slice(&self.local_port.to_be_bytes());
        segment.extend_from_slice(&self.remote_port.to_be_bytes());
        segment.extend_from_slice(&self.sequence.to_be_bytes());
        segment.extend_from_slice(&self.acknowledgement.to_be_bytes());
        segment.push(5 << 4); // data offset: 5 words, no options
        segment.push(flags);
        segment.extend_from_slice(&64240u16.to_be_bytes()); // window size
        segment.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
        segment.extend_from_slice(&0u16.to_be_bytes()); // urgent pointer
        segment.extend_from_slice(payload);
        let checksum = tcp_checksum(ip(), self.remote_ip, &segment);
        segment[16..18].copy_from_slice(&checksum.to_be_bytes());
        segment
    }

    fn send(&self, flags: u8, payload: &[u8]) {
        let segment = self.build(flags, payload);
        send_ipv4(self.remote_ip, 6, &segment);
    }

    fn wait_segment(&self) -> Option<Segment> {
        let local_port = self.local_port;
        let remote_port = self.remote_port;
        let source = self.remote_ip;
        let frame = wait_for(
            |frame| {
                parse_ipv4(frame).is_some_and(|packet| {
                    if packet.protocol != 6 || packet.source != source || packet.payload.len() < 20
                    {
                        return false;
                    }
                    u16::from_be_bytes([packet.payload[0], packet.payload[1]]) == remote_port
                        && u16::from_be_bytes([packet.payload[2], packet.payload[3]]) == local_port
                })
            },
            TCP_TIMEOUT,
        )?;
        let packet = parse_ipv4(&frame)?;
        let header = packet.payload;
        let header_length = (usize::from(header[12] >> 4) * 4).min(header.len());
        Some(Segment {
            sequence: u32::from_be_bytes([header[4], header[5], header[6], header[7]]),
            flags: header[13],
            payload: header[header_length..].to_vec(),
        })
    }
}

#[derive(Debug)]
struct NetIoError(ErrorKind);

impl core::fmt::Display for NetIoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "network I/O error: {:?}", self.0)
    }
}

impl core::error::Error for NetIoError {}

impl embedded_io::Error for NetIoError {
    fn kind(&self) -> ErrorKind {
        self.0
    }
}

/// Adapts the synchronous TCP client to the blocking `embedded_io` traits used
/// by `embedded-tls`. Incoming payloads are `ACK`ed as they are consumed, which
/// also gives plain `HTTP` one ordered byte stream instead of loose segments.
struct TcpStream {
    connection: Connection,
    pending: Vec<u8>,
    pending_pos: usize,
    eof: bool,
}

impl TcpStream {
    fn new(connection: Connection) -> Self {
        Self {
            connection,
            pending: Vec::new(),
            pending_pos: 0,
            eof: false,
        }
    }

    /// Waits for the next usable `TCP` segment, `ACK`ing payload as it arrives.
    fn pump(&mut self) -> Result<(), NetIoError> {
        let segment = self
            .connection
            .wait_segment()
            .ok_or(NetIoError(ErrorKind::TimedOut))?;
        if segment.flags & TCP_RST != 0 {
            return Err(NetIoError(ErrorKind::ConnectionReset));
        }

        if !segment.payload.is_empty() && segment.sequence == self.connection.acknowledgement {
            let length = segment.payload.len() as u32;
            self.pending = segment.payload;
            self.pending_pos = 0;
            self.connection.acknowledgement = self.connection.acknowledgement.wrapping_add(length);
            self.connection.send(TCP_ACK, &[]);
        }

        if segment.flags & TCP_FIN != 0 {
            self.connection.acknowledgement = self.connection.acknowledgement.wrapping_add(1);
            self.connection.send(TCP_FIN | TCP_ACK, &[]);
            self.eof = true;
        }
        Ok(())
    }
}

impl embedded_io::ErrorType for TcpStream {
    type Error = NetIoError;
}

impl embedded_io::Read for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, NetIoError> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.pending_pos >= self.pending.len() {
            if self.eof {
                return Ok(0);
            }
            self.pump()?;
        }
        let available = &self.pending[self.pending_pos..];
        let count = available.len().min(buf.len());
        buf[..count].copy_from_slice(&available[..count]);
        self.pending_pos += count;
        Ok(count)
    }
}

impl embedded_io::Write for TcpStream {
    fn write(&mut self, buf: &[u8]) -> Result<usize, NetIoError> {
        if buf.is_empty() {
            return Ok(0);
        }
        // Keep each outgoing segment below the NIC's 2048-byte TX buffer.
        let count = buf.len().min(1200);
        self.connection.send(TCP_PSH | TCP_ACK, &buf[..count]);
        self.connection.sequence = self.connection.sequence.wrapping_add(count as u32);
        Ok(count)
    }

    fn flush(&mut self) -> Result<(), NetIoError> {
        Ok(())
    }
}

/// A deliberately small, non-cryptographic RNG used only for the TLS client
/// nonce and ephemeral key pair. Seeding mixes RDTSC, the PIT and the MAC; this
/// is adequate for a pre-alpha experimental browser but is not a real CSPRNG.
struct KernelRng {
    state: u64,
}

impl KernelRng {
    fn new() -> Self {
        let low: u32;
        let high: u32;
        // SAFETY: `rdtsc` only reads the timestamp counter; it has no memory
        // operands and is available in ring 0 on the x86_64 target.
        unsafe {
            core::arch::asm!(
                "rdtsc",
                out("eax") low,
                out("edx") high,
                options(nomem, nostack)
            );
        }
        let mut seed = (u64::from(high) << 32) | u64::from(low);
        seed ^= interrupts::ticks().wrapping_mul(0x9E37_79B9_7F4A_7C15);
        for (index, byte) in MAC.iter().enumerate() {
            seed ^= u64::from(*byte).wrapping_shl((index % 8) as u32 * 8);
        }
        Self { state: seed | 1 }
    }

    fn next(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value >> 12;
        value ^= value << 25;
        value ^= value >> 27;
        self.state = value;
        value.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

impl RngCore for KernelRng {
    fn next_u32(&mut self) -> u32 {
        self.next() as u32
    }

    fn next_u64(&mut self) -> u64 {
        self.next()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(8) {
            let bytes = self.next().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl CryptoRng for KernelRng {}

/// A parsed absolute or bare-host URL.
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    fn authority(&self) -> String {
        let default_port = (self.scheme == "http" && self.port == 80)
            || (self.scheme == "https" && self.port == 443);
        if default_port {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    fn is_tls(&self) -> bool {
        self.scheme == "https"
    }
}

/// Parses `http://host:port/path`, `https://...` or a bare `host/path`.
pub fn parse_url(input: &str) -> Option<Url> {
    let input = input.trim();
    let without_fragment = input.split_once('#').map_or(input, |(before, _)| before);

    let (scheme, rest) = if let Some(rest) = without_fragment.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = without_fragment.strip_prefix("http://") {
        ("http", rest)
    } else {
        ("http", without_fragment)
    };

    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let mut authority = &rest[..authority_end];
    // User information is accepted but never used: this browser has no auth UI.
    if let Some((_, after)) = authority.rsplit_once('@') {
        authority = after;
    }

    let default_port = if scheme == "https" { 443 } else { 80 };
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let close = bracketed.find(']')?;
        let host = bracketed[..close].to_string();
        let tail = &bracketed[close + 1..];
        let port = match tail.strip_prefix(':') {
            Some(port) => port.parse().ok()?,
            None => default_port,
        };
        (host, port)
    } else {
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port))
                if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                (host, port.parse().ok()?)
            }
            _ => (authority, default_port),
        };
        (host.to_string(), port)
    };
    if host.is_empty() {
        return None;
    }

    let rest_path = &rest[authority_end..];
    let path = if rest_path.is_empty() {
        String::from("/")
    } else if rest_path.starts_with('/') {
        rest_path.to_string()
    } else {
        format!("/{rest_path}")
    };

    Some(Url {
        scheme: scheme.into(),
        host,
        port,
        path,
    })
}

/// Resolves a `Location` header against the URL that produced it.
fn absolute_url(base: &Url, location: &str) -> String {
    let location = location.trim();
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    if let Some(rest) = location.strip_prefix("//") {
        return format!("{}:{rest}", base.scheme);
    }
    if location.starts_with('?') {
        return format!(
            "{}://{}{}{}",
            base.scheme,
            base.authority(),
            base.path,
            location
        );
    }
    if location.starts_with('/') {
        return format!("{}://{}{}", base.scheme, base.authority(), location);
    }
    let directory = base.path.rsplit_once('/').map_or("", |(prefix, _)| prefix);
    format!(
        "{}://{}{}/{}",
        base.scheme,
        base.authority(),
        directory,
        location
    )
}

/// A fetched HTTP response with headers already decoded.
pub struct Response {
    pub status: u16,
    pub status_line: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub final_url: String,
}

impl Response {
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Reconstructs the wire form for the shell's existing response printer.
    pub fn raw_text(&self) -> String {
        let mut text = String::new();
        text.push_str(&self.status_line);
        text.push_str("\r\n");
        for (key, value) in &self.headers {
            text.push_str(key);
            text.push_str(": ");
            text.push_str(value);
            text.push_str("\r\n");
        }
        text.push_str("\r\n");
        text.push_str(&self.body_text());
        text
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Decodes `Transfer-Encoding: chunked`, which HTTP/1.1 servers use when the
/// response length is not known in advance.
fn decode_chunked(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut position = 0;
    while position < input.len() {
        let line_end =
            find_bytes(&input[position..], b"\r\n").map_or(input.len(), |offset| position + offset);
        let line = String::from_utf8_lossy(&input[position..line_end]);
        let size_text = line.split(';').next().unwrap_or("0").trim();
        let size = usize::from_str_radix(size_text, 16).unwrap_or(0);
        position = line_end + 2;
        if size == 0 || position + size > input.len() {
            break;
        }
        output.extend_from_slice(&input[position..position + size]);
        position += size;
        if input.get(position..position + 2) == Some(b"\r\n") {
            position += 2;
        } else {
            break;
        }
    }
    output
}

fn parse_response(raw: &[u8], url: &Url) -> Response {
    let head_end = find_bytes(raw, b"\r\n\r\n")
        .map(|offset| offset + 4)
        .or_else(|| find_bytes(raw, b"\n\n").map(|offset| offset + 2))
        .unwrap_or(raw.len());
    let head = String::from_utf8_lossy(&raw[..head_end]);
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("").trim().to_string();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);

    let mut headers = Vec::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.push((key.trim().to_string(), value.trim().to_string()));
        }
    }

    let mut body = raw[head_end..].to_vec();
    let chunked = headers.iter().any(|(key, value)| {
        key.eq_ignore_ascii_case("transfer-encoding") && value.contains("chunked")
    });
    if chunked {
        body = decode_chunked(&body);
    }

    Response {
        status,
        status_line,
        headers,
        body,
        final_url: format!("{}://{}{}", url.scheme, url.authority(), url.path),
    }
}

fn read_all<R: embedded_io::Read>(reader: &mut R, limit: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    while bytes.len() < limit {
        match reader.read(&mut buffer) {
            // A timeout or a TLS close without `close_notify` is normal once
            // the server has sent the document; keep what we received.
            Ok(0) | Err(_) => break,
            Ok(count) => bytes.extend_from_slice(&buffer[..count]),
        }
    }
    bytes
}

/// Fetches one URL, without following redirects.
fn fetch_once(url: &Url) -> Result<Response, String> {
    if !is_up() {
        return Err(String::from("No network adapter"));
    }
    let address = parse_address(&url.host)
        .or_else(|| resolve(&url.host))
        .ok_or_else(|| format!("Could not resolve {}", url.host))?;
    let connection = Connection::connect(address, url.port)
        .ok_or_else(|| format!("Could not connect to {}:{}", url.host, url.port))?;

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: RustOS/0.1 (pre-alpha)\r\nAccept: */*\r\nAccept-Language: en\r\nConnection: close\r\n\r\n",
        url.path,
        url.authority()
    );

    let raw = if url.is_tls() {
        let mut stream = TcpStream::new(connection);
        // The read buffer must fit the largest possible TLS record (16640 B).
        let mut read_buffer = vec![0u8; 16_640];
        let mut write_buffer = vec![0u8; 4096];
        let mut tls = TlsConnection::<_, Aes128GcmSha256>::new(
            &mut stream,
            &mut read_buffer,
            &mut write_buffer,
        );
        let config = TlsConfig::new()
            .with_server_name(&url.host)
            // Most certificates are still RSA-signed. `NoVerify` means we never
            // actually check the signature, so no RSA dependency is required.
            .enable_rsa_signatures();
        let provider = UnsecureProvider::new::<Aes128GcmSha256>(KernelRng::new());
        let context = TlsContext::new(&config, provider);
        tls.open(context)
            .map_err(|error| format!("TLS handshake failed ({error:?})"))?;
        tls.write_all(request.as_bytes())
            .map_err(|error| format!("TLS request failed ({error:?})"))?;
        tls.flush()
            .map_err(|error| format!("TLS flush failed ({error:?})"))?;
        read_all(&mut tls, RESPONSE_LIMIT)
    } else {
        let mut stream = TcpStream::new(connection);
        stream
            .write_all(request.as_bytes())
            .map_err(|error| format!("Request failed: {error}"))?;
        read_all(&mut stream, RESPONSE_LIMIT)
    };

    Ok(parse_response(&raw, url))
}

/// Fetches a URL over HTTP or HTTPS, following up to [`MAX_REDIRECTS`] redirects.
///
/// # Security
///
/// HTTPS encrypts the connection but does **not** verify the server
/// certificate: this kernel has neither a trust store nor a clock. It protects
/// against passive observers only and is vulnerable to an active attacker.
pub fn fetch_url(input: &str) -> Result<Response, String> {
    let mut url = parse_url(input).ok_or_else(|| String::from("Invalid URL"))?;
    for _ in 0..=MAX_REDIRECTS {
        let response = fetch_once(&url)?;
        if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            let location = response
                .header("location")
                .ok_or_else(|| String::from("Redirect without Location header"))?;
            let next = absolute_url(&url, location);
            url = parse_url(&next).ok_or_else(|| String::from("Invalid redirect URL"))?;
            continue;
        }
        return Ok(response);
    }
    Err(String::from("Too many redirects"))
}

/// Parses a dotted-quad IPv4 address.
pub fn parse_address(text: &str) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut count = 0;
    for part in text.split('.') {
        if count == 4 {
            return None;
        }
        octets[count] = part.parse().ok()?;
        count += 1;
    }
    (count == 4).then_some(octets)
}

# Networking

RustOS speaks IPv4 on the emulated RTL8139 that QEMU attaches for
`-nic user,model=rtl8139` (the launcher and the test scripts pass that flag).
At boot the kernel runs a **DHCP** client, so it takes whatever address the
network hands out; if no server answers within half a second it falls back to
the fixed defaults you would see on any QEMU guest, which QEMU's user-mode
network ("SLIRP") always provides:

| Setting | Value |
| --- | --- |
| Guest address | `10.0.2.15/24` |
| Gateway | `10.0.2.2` |
| DNS proxy | `10.0.2.3` |
| MAC | `52:54:00:12:34:56` |

The stack is split across two modules:

- `kernel/src/pci.rs` scans PCI bus 0 through the `0xCF8`/`0xCFC` configuration
  ports, finds the adapter, and enables its I/O space and bus-master bit.
- `kernel/src/net.rs` is the driver and the protocol stack: the RTL8139 receive
  ring and four transmit descriptors, Ethernet framing, ARP, IPv4, ICMP echo,
  UDP, a DNS resolver, a synchronous TCP client, `embedded-tls` for HTTPS, and a
  small URL/HTTP client behind `fetch_url`.

The driver is **polled** from the shell rather than interrupt-driven, so the
whole stack stays single-threaded and the receive path is a plain "read the ring,
advance `CAPR`" loop. DMA buffers are carved out of the kernel heap, which lives
in the bootloader's physical-memory window, so the physical address the NIC needs
is simply `virtual_address - physical_memory_offset`.

Try it from the shell:

```
rustos> net
adapter: RTL8139, I/O 0xc000, IRQ 11
address: 10.0.2.15
mac:     52:54:00:12:34:56
gateway: 10.0.2.2
dns:     10.0.2.3
rustos> ping 10.0.2.2
PING 10.0.2.2 ... reply in 0 ms
rustos> dns example.com
resolving example.com ... example.com -> 93.184.216.34
rustos> http http://example.com/
GET http://example.com/
final URL: http://example.com/
HTTP/1.1 200 OK
...
rustos> http https://example.com/
GET https://example.com/
final URL: https://example.com/
HTTP/1.1 200 OK
...
```

`ping`, `dns` and `http` reach beyond the gateway only when the host has
connectivity; the gateway itself always answers because SLIRP implements the
virtual host locally. Off-subnet traffic is sent to the gateway's MAC while
keeping the real destination in the IP header, because SLIRP only answers ARP for
the addresses it owns.

The Windows XP-style **Browser** window has an editable address bar: type a URL,
press **Enter** (or click **Go**) and it fetches the page with `net::fetch_url`,
converts the HTML to text (`kernel/src/browser.rs`) and shows the title, status
line and body. The fetch blocks the UI for its duration, then returns to normal;
the arrow and Page Up/Down keys scroll the rendered page.

`fetch_url` accepts `http://`, `https://` and bare `host/path` addresses, follows
up to five redirects, decodes `Transfer-Encoding: chunked`, and does not ask for
compression (so most pages arrive as plain HTML). HTTPS uses TLS 1.3 through the
`embedded-tls` crate. Because the kernel has neither a root certificate store
nor a wall clock, **certificate and hostname checks are disabled**: the
connection is encrypted against passive observers but is vulnerable to an
active man-in-the-middle. Treat HTTPS as a pre-alpha experiment, not a secure
browser. Sites that require TLS 1.2 only, JavaScript, CSS layout, forms or
cookies still will not work properly.

## Testing the network

The smoke test always checks the adapter and pings the gateway. Pass `-Network`
to also resolve a name and fetch pages over HTTP and HTTPS, which needs the host
to have internet access:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\smoke-test.ps1 -Network
```

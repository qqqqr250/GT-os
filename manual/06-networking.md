# Networking

GTos speaks IPv4 on the emulated **Intel 82540EM** NIC that VirtualBox provides
with `--nic1 nat --nic-type1 82540EM` (attached by `scripts\virtualbox.ps1`).
`net` prints which adapter was found. At boot the kernel runs a **DHCP** client,
so it takes whatever address the network hands out; if no server answers within
half a second it falls back to the fixed defaults of VirtualBox's NAT network:

| Setting | Value |
| --- | --- |
| Guest address | `10.0.2.15/24` |
| Gateway | `10.0.2.2` |
| DNS proxy | `10.0.2.3` |
| MAC | `52:54:00:12:34:56` |

The kernel also recognises the RTL8139, but the VirtualBox VM uses the E1000
driver. The stack is split across two modules:

- `kernel/src/pci.rs` scans PCI bus 0 through the `0xCF8`/`0xCFC` configuration
  ports, finds the adapter, and enables its I/O, memory-space and bus-master bits.
- `kernel/src/net.rs` holds both drivers and the protocol stack: the RTL8139
  linear ring, the E1000 descriptor rings (32 RX / 8 TX), and above them Ethernet
  framing, ARP, IPv4, ICMP echo, UDP, a DNS resolver, a synchronous TCP client,
  `embedded-tls` for HTTPS, and a small URL/HTTP client behind `fetch_url`.

Both drivers are **polled** from the shell rather than interrupt-driven, so the
whole stack stays single-threaded. DMA buffers are carved out of the kernel heap,
which lives in the bootloader's physical-memory window, so the physical address
the NIC needs is simply `virtual_address - physical_memory_offset`. The E1000's
MMIO registers are reached through the same window at
`physical_memory_offset + BAR0`.

Try it from the shell:

```
gtos> net
adapter: Intel E1000, MMIO 0xf0000000, IRQ 11
address: 10.0.2.15
mac:     52:54:00:12:34:56
gateway: 10.0.2.2
dns:     10.0.2.3
gtos> ping 10.0.2.2
PING 10.0.2.2 ... reply in 0 ms
gtos> dns example.com
resolving example.com ... example.com -> 93.184.216.34
gtos> http http://example.com/
GET http://example.com/
final URL: http://example.com/
HTTP/1.1 200 OK
...
gtos> http https://example.com/
GET https://example.com/
final URL: https://example.com/
HTTP/1.1 200 OK
...
```

`ping`, `dns` and `http` reach beyond the gateway only when the host has
connectivity; the gateway itself always answers because VirtualBox's NAT engine
implements the virtual host locally. Off-subnet traffic is sent to the gateway's
MAC while keeping the real destination in the IP header, because the NAT engine
only answers ARP for the addresses it owns.

GTos has no graphical desktop and no browser application. Web access is
available through the shell's `http` command, which uses the `net::fetch_url`
stack described below and prints the response body as text.

`fetch_url` accepts `http://`, `https://` and bare `host/path` addresses, follows
up to five redirects, decodes `Transfer-Encoding: chunked`, and does not ask for
compression (so most pages arrive as plain HTML). Response bodies are decoded as
UTF-8 when valid, and as Latin-1/Windows-1252 when the server declares that
charset, so accented characters and smart quotes stay readable. HTTPS uses TLS 1.3 through the
`embedded-tls` crate. Because the kernel has neither a root certificate store
nor a wall clock, **certificate and hostname checks are disabled**: the
connection is encrypted against passive observers but is vulnerable to an
active man-in-the-middle. Treat HTTPS as a pre-alpha experiment, not a secure
browser. Sites that need TLS 1.2 only, JavaScript, full CSS layout, working
forms or cookies will still look incomplete or not work properly. External CSS,
images and scripts are not fetched separately, so most pages keep their default
styling rather than the site's own design.

## Checking the network by hand

There is no automated network check. In the VM, run `net` to see the adapter and
address, then `ping 10.0.2.2` to confirm the NAT gateway answers. With host
internet access, `dns example.com` and `http http://example.com/` exercise the
resolver and HTTP client from the shell.

# Roadmap

- Per-process address spaces (today every program shares one user window).
- A real page-frame allocator for the regions the heap does not use.
- Timer-driven sleep and a scheduler (the PIT tick is already there).
- Seek and write syscalls, plus per-process descriptor tables (schemes are read-only snapshots today).
- An ELF loader in place of position-dependent flat binaries.
- Streaming scheme servers and IPC instead of in-kernel resource snapshots.
- AHCI and USB mass storage alongside ATA PIO.
- Verified TLS (a root store and clock; today's HTTPS is encrypted but unverified).
- IPv6 for the network stack, plus retry/congestion handling in the TCP client.
- More exception context (error codes, stack traces against a symbol map).

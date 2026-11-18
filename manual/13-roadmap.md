# Roadmap

- Per-process address spaces (today every program shares one user window).
- A real page-frame allocator for the regions the heap does not use.
- Timer-driven sleep and a scheduler (the PIT tick is already there).
- More system calls (open/read/seek) and an ELF loader in place of flat binaries.
- AHCI and USB mass storage alongside ATA PIO.
- IPv6 and TLS for the network stack (HTTP is still plaintext).
- More exception context (error codes, stack traces against a symbol map).

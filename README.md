# GTos

A small, bootable x86_64 operating system written from scratch in Rust — no
Linux, no `std`, no libc.

> **Pre-alpha:** GTos is an experimental hobby OS. Expect incomplete features,
> bugs and breaking changes; it is not ready for daily use. The browser's HTTPS
> support is encrypted but does not verify certificates.

![GTos running in QEMU](docs/screenshot.png)

## The GTos Manual

The manual lives in [`manual/`](manual/), which is its authoritative source.

- [Welcome to GTos!](manual/01-welcome.md)

**The Basics**

- [Getting started](manual/02-getting-started.md)
- [Architecture](manual/03-architecture.md)

**The System**

- [Desktop environment](manual/04-desktop.md)
- [User mode (ring 3)](manual/05-user-mode.md)
- [Networking](manual/06-networking.md)
- [Audio](manual/07-audio.md)
- [Disks and the FAT16 filesystem](manual/08-disks.md)

**Boot and packaging**

- [Bootable ISO and Ventoy](manual/09-bootable-iso.md)
- [Linux package](manual/10-linux-package.md)

**The Rest**

- [Testing](manual/11-testing.md)
- [Troubleshooting](manual/12-troubleshooting.md)
- [Roadmap](manual/13-roadmap.md)
- [License and credits](manual/14-license-and-credits.md)

## License

GTos is released under the [MIT License](LICENSE).

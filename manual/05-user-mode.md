# User mode (ring 3)

Programs run with real hardware isolation. `kernel/src/usermode.rs` installs its
own GDT (null, kernel code/data, user code/data and a TSS), points the TSS
`privilege_stack_table[0]` at a dedicated 16 KiB ring-0 stack and its
`interrupt_stack_table[0]` at a separate 16 KiB double-fault stack (post-06), and
maps a private 64 KiB program window plus a 16 KiB stack at PML4 entry 1
(`0x0000_0080_0000_0000`), far from the kernel's identity map and the
physical-memory window.

The only way in is `int 0x80`, installed as a DPL-3 gate. The handler is written
in `global_asm!`: it saves the user registers, calls the Rust dispatcher and
`iretq`s back. Two system calls exist today — `exit(status)` (0) and
`write(fd, buffer, length)` (1) — which is enough for a program to print and
return. A fault taken in ring 3 is contained: the exception handler notices the
faulting `CS` has RPL 3, reports it and terminates the program, handing control
back to the shell instead of halting the machine.

`run` with no argument executes a hand-assembled demo program built in memory;
`run HELLO.BIN` loads a flat binary off the data disk. A flat binary is linked
against the user base, so it is position-dependent by design:

```
rustos> run
running demo program (71 bytes) in ring 3 ...
Hello from ring 3! (via int 0x80)
demo program exited with status 0
rustos> run HELLO.BIN
running HELLO.BIN (77 bytes) in ring 3 ...
Hello from HELLO.BIN running in ring 3!
HELLO.BIN exited with status 0
```

Because `exit_user` returns to the kernel with `ret` rather than `iretq`, it
restores the kernel's saved `RFLAGS` by hand — without that, the `int 0x80`
interrupt gate would leave interrupts disabled and the shell's `hlt` would never
wake again.

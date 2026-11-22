# Verifying GTos

GTos has **no automated test suite**. There is no test runner and no
end-to-end test script: verification is manual, in a VirtualBox VM.

Build first, then create and boot the VM. The build script watches both
`kernel/src` and the guest sources, so kernel-only changes are rebuilt into the
boot image before the VDIs are recreated:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build.ps1
powershell -ExecutionPolicy Bypass -File scripts\virtualbox.ps1 -Start
```

Once the VM has booted, check the following by hand:

- **Boot** – the boot log appears directly on the text console, with the GT logo
  strip above it. Nothing else starts: there is no desktop, launcher or window.
  The boot log is mirrored to COM1. See [Console shell](04-console.md).
- **Shell** – at the `GTos>` prompt try `help`, `mem`, `heap`, `uptime`, `net`,
  `ping 10.0.2.2`, `disks` and `ls`. **Up**/**Down** recall history and **Tab**
  completes names.
- **Audio** – run `play BEEP.WAV`; the shell should print
  `playing BEEP.WAV (... bytes) ... done`. See [Audio](07-audio.md).
- **User mode** – `run`, `run HELLO.BIN`, `run HELLO.ELF`, `run RUSTDEMO.ELF`,
  `run CABI.ELF`, `run scheme`, `run fork`, `run preempt`, `run sleep`,
  `run exec` and `session` should each print their banner message and exit with
  status 0.
  `run CABI.ELF` should additionally print `hello`, `cabi ok` and `crt ok` (the
  last from an `atexit` handler). `run preempt` should
  interleave `P` and `C` output; `run sleep` should print the parent's message
  before the child's. `run RUSTDEMO.ELF` should additionally print
  `monotonic clock advanced`, `wall clock available`, `file write/read ok`,
  `heap vector: [1, 2, 3, 4]`, `guest allocator ok`, `framebuffer device ok`,
  `window damage ok`, `window stacking ok`, `window decorations ok`,
  `window shm ok`, `window ipc ok`, `window compositor ok`, `widget toolkit ok`,
  `widget layout ok`, `widget view ok`, `widget nested ok`, `widget editor ok`,
  `shell panel ok`, `shell apps ok`, `shell focus ok`, `shell actions ok`,
  `shell launch ok`, `shared memory ok`,
   `streaming ipc ok`, `socketpair ok`, `ioctl ok`, `stream file ok`, `fat writeback ok`,
  `directory stream ok`,
  `vfs ok`, `scheme server ok`,
  `scheme shm ok`,
  `waiting for input event`,
  `input press: A shift`,
  `input release: A`, `input device ok`, `handler entered` and `signal delivered`
  before exiting with status 0. The signal checks continue with
  `masked handler entered`/`signal mask ok` for blocked-then-unblocked delivery,
  `signal ignore ok` for `SIG_IGN`, `queued first handler`/`queued handler
  entered`/`signal queue ok` for delivery after `sigreturn`,
  `child handler entered`/`signal cross process ok` for a parent signalling a
   forked child through a pipe, `signal default action ok`, `signal kill ok`,
   `rt signal queue ok`, and `fault signal ok` for fatal dispositions, 64-bit
   RT mask/pending delivery, and a handled ring-3 page fault observed by `waitpid`.
   The input test waits up to
  supply the events from the host, run this as soon as `waiting for input event`
  appears:

  ```powershell
  & 'D:\virtualbox\VBoxManage.exe' controlvm GTos keyboardputscancode 2a 1e 9e aa
  ```

  That is left Shift down, `a` down, `a` up, left Shift up, so the guest sees a
  shifted `A` press followed by its release. The shell also receives the `A` on
  its own byte queue after the guest exits. See [User mode](05-user-mode.md).
- **Session manager** – `session` should print `session manager starting`, start
  the `term` autostart target (`Hello from HELLO.BIN running in ring 3!`), print
  `session autostart ok`, then start `SERVICE.ELF` and observe two restarts from
  its nonzero exit (`managed service ready`/`managed service exiting`,
  `session service demo restart 1 status 7`, then restart 2 and
  `session service demo stopped status 7 restarts 2`). It should then print
  `session services ok`, verify its service bus with a forked client (a second
  `Hello from HELLO.BIN running in ring 3!`, then `session bus ok` and
  `session kcm ok`/`session kcm theme: theme=dark`), and reach
  `session waiting for input`. Supply it with:

  ```powershell
  & 'D:\virtualbox\VBoxManage.exe' controlvm GTos keyboardputscancode 1c 9c
  ```

  The session should then print `session request: shutdown`,
  `session manager shutdown ok` and `SESSION.ELF exited with status 0`.
  `cat SESSION.ST` should report `stopping`, and `cat SERVICES.ST` should show
  the `demo` service as `stopped` with two restarts.

The VM is stopped from the VirtualBox UI, or with
`VBoxManage controlvm GTos poweroff`: `shutdown` in the shell halts the guest CPU
but does not power the VM off.

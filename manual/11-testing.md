# Verifying GTos

GTos has **no automated test suite**. There is no test runner and no
end-to-end test script: verification is manual, in a VirtualBox VM.

Build first, then create and boot the VM:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build.ps1
powershell -ExecutionPolicy Bypass -File scripts\virtualbox.ps1 -Start
```

Once the desktop is up, check the following by hand:

- **Boot** – the Luna-blue desktop, taskbar and Start menu appear; the boot log
  is mirrored to COM1.
- **Browser** – press **F1**, type a URL and press Enter. The page renders with
  clickable links; **Tab** cycles through them and **Enter** follows the
  focused one.
- **File manager** – press **F2** (or open **My Computer** from the Start menu).
  List, search and sort the files; click a row twice or press **Enter** to open
  `README.TXT`.
- **Start menu** – press the **Windows key** to toggle it. Open windows from the
  menu, raise them from the taskbar and close them.
- **Shell** – send a byte over COM1 (a bare Enter is enough) to hand the console
  to the shell, then try `help`, `mem`, `heap`, `uptime`, `net`,
  `ping 10.0.2.2`, `disks` and `ls`.
- **Audio** – run `play BEEP.WAV`; the shell should print
  `playing BEEP.WAV (... bytes) ... done`. See [Audio](07-audio.md).

The VM is stopped from the VirtualBox UI, or with
`VBoxManage controlvm GTos poweroff`: `shutdown` in the shell halts the guest CPU
but does not power the VM off.

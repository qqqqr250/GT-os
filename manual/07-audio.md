# Audio

GTos plays sound through an emulated **AC'97** codec (PCI `8086:2415`). The
driver in `kernel/src/audio.rs` finds the device on the PCI bus, programs the
native-audio mixer and bus-mastering registers, and feeds the codec from a
32-entry buffer descriptor list: it allocates a PCM buffer, points the
descriptors at it and lets the codec's DMA engine read it out.

`beep` and `tone <hz> [ms]` synthesise a square wave directly, while
`play <file>` reads a PCM WAV file off the FAT16 data disk, parses its header and
streams the samples. The shell waits for playback to finish (the driver polls the
codec's current-index register) so the prompt returns only when the sound is
done.

`scripts\virtualbox.ps1` creates the VM with host audio disabled, so there is no
host speaker output to hear by default. Verify playback from the guest instead:

```powershell
gtos> play BEEP.WAV
playing BEEP.WAV (... bytes) ... done
```

`done` means the WAV was parsed, DMA was set up and the codec reported playback
complete. If you want to hear the guest's audio, enable audio in the VM's
VirtualBox settings (Audio → Enable Audio) and start the VM again.

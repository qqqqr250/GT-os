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

QEMU's audio backend is selectable on the launcher:

```powershell
scripts\run.ps1 --audio wav       # default: record playback to audio.wav
scripts\run.ps1 --audio dsound    # live output through DirectSound
scripts\run.ps1 --audio none      # no audio device
```

`wav` is the default because it always works, including on hosts where the
DirectSound backend refuses to open a device; `dsound` gives you live sound when
your host supports it. The smoke test uses the `wav` backend and asserts that the
captured file contains real (non-zero) samples, which is how the driver is
verified without a speaker.

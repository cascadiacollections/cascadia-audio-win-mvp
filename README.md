# cascadia-audio-win-mvp

Proof of concept: live SHOUTcast/Icecast radio stream decoded in Rust
(Symphonia + cpal/WASAPI) and played in a WinUI 3 app via P/Invoke.
No Media Foundation. No NAudio. No Windows.Media.Playback.

## What this proves
- Symphonia + cpal (WASAPI backend) can decode and play a live HTTP audio
  stream on Windows
- ICY metadata stripping works without corrupting MP3/AAC frame sync
- A flat C ABI from a Rust cdylib is consumable from C# via [LibraryImport]
  with no COM, no WinRT, no managed wrapper libraries
- tokio runtime + cpal audio callback coexist without deadlock

## Prerequisites
- Rust stable (msvc toolchain): `rustup default stable-msvc`
- Visual Studio 2022 with "Desktop development with C++" workload (for MSVC
  linker) and "Windows application development" workload (for WinUI 3)
- Windows App SDK 1.5+
- .NET 9 SDK

## Build
```powershell
# Rust DLL
cd rust
cargo build --release --target x86_64-pc-windows-msvc

# Copy DLL (or let MSBuild do it)
copy target\x86_64-pc-windows-msvc\release\cascadia_audio_win_mvp.dll `
     ..\windows\CascadiaAudioMvp\native\

# WinUI app
cd ..\windows
dotnet build -c Release
# or open CascadiaAudioMvp.sln in Visual Studio and F5
```

MSBuild runs the cargo build and DLL copy automatically on BeforeBuild.

## Linux (Rust crate only)
The WinUI host is Windows-only, but the Rust crate can compile on Linux.

```bash
sudo dnf install pkgconf-pkg-config alsa-lib-devel
cd rust
cargo build --release
```

## Known failure mode to watch for
Symphonia needs enough buffered bytes to probe the stream format before
decoding begins. If you get a probe failure on first run, the fix is to
pre-buffer 64KB in ChannelSource before returning the first Read bytes.

## Known limitations (deferred to full engine)
- No pause, only stop
- No seek
- No volume control UI (Rust side has AtomicU32 volume, not wired to UI)
- No error recovery / reconnect
- No MediaSession / SMTC integration (system transport controls)
- No background playback

## Relationship to cascadia-audio-mvp (Android)
The ChannelSource ICY stripping code and Symphonia pipeline are identical.
The only platform delta is OboeSink (Android) vs cpal WASAPI (Windows).
Both MVPs validate the same core engine. If both play audio, the
platform-agnostic core is ready to extract into the cascadia-audio crate.

## Next
If audio plays on both Android and Windows: extract shared core into
`cascadiacollections/cascadia-audio` workspace crate, add AudioHandle
state machine, wire UniFFI for Kotlin/Swift and P/Invoke C ABI as
separate crate features.

# cascadia-audio-win-mvp

Proof of concept: live SHOUTcast/Icecast radio stream decoded in Rust
(cpal backend + FFmpeg for AAC/HE-AAC) and played in an Avalonia desktop app
via P/Invoke. No Media Foundation. No NAudio. No Windows.Media.Playback.

## What this proves
- Symphonia + cpal can decode and play a live HTTP audio stream with a single
  Rust engine on Linux and Windows
- ICY metadata stripping works without corrupting MP3/AAC frame sync
- A flat C ABI from a Rust cdylib is consumable from C# via [LibraryImport]
  with no COM, no managed wrapper libraries
- tokio runtime + cpal audio callback coexist without deadlock

## Prerequisites
- Rust stable
- .NET 9 SDK
- FFmpeg CLI installed and on `PATH` (required for AAC/HE-AAC decoding)
- Linux audio build deps (Fedora): `sudo dnf install pkgconf-pkg-config alsa-lib-devel ffmpeg`
- Linux audio build deps (Ubuntu): `sudo apt-get install -y pkg-config libasound2-dev ffmpeg`

## Build (Linux)
```bash
cd rust
cargo build --release

cd ../avalonia/CascadiaAudioMvp
dotnet build -c Release
dotnet run -c Release
```

MSBuild in the Avalonia project runs the Rust build automatically before app
build and copies the native library into the app output folder.

## Build (Windows)
```powershell
cd rust
cargo build --release --target x86_64-pc-windows-msvc

cd ..\avalonia\CascadiaAudioMvp
dotnet build -c Release
dotnet run -c Release
```

## Test workflow (local + CI)
Run Rust unit and integration smoke tests:
```bash
cd rust
cargo test
```

Run Avalonia host/service tests:
```bash
dotnet test avalonia/CascadiaAudioMvp.Tests/CascadiaAudioMvp.Tests.csproj -c Release
```

CI executes the same commands in `.github/workflows/tests.yml`.

## Initial coverage targets
- Rust: cover helper and stream-buffering logic (`remix_channels`,
  `ChannelSource`, `prebuffer_stream`) plus C ABI smoke checks.
- .NET: cover managed host service interactions (`AudioService`) without UI
  automation.

## Known failure mode to watch for
The AAC path now uses FFmpeg so it can handle HE-AAC and ADTS framing, but
stream startup still benefits from pre-buffering. If playback stalls on first
launch, wait a moment for the initial audio buffer to accumulate before judging
it a failure.

## Playback state and telemetry
- Native state machine: `Stopped`, `Starting`, `Buffering`, `Playing`,
  `Reconnecting`, `Stopping`, `Error`
- Automatic reconnect retries for transient stream failures
- Native telemetry counters include decode stats plus underrun count, estimated
  buffered latency, and reconnect attempts

## Known limitations (deferred to full engine)
- No pause, only stop
- No seek
- No volume control UI (Rust side has AtomicU32 volume, not wired to UI)
- No MediaSession / SMTC integration (system transport controls)
- No background playback

## Relationship to cascadia-audio-mvp (Android)
The ChannelSource ICY stripping code and Symphonia pipeline are identical.
The only platform delta is OboeSink (Android) vs cpal host backend
(WASAPI/ALSA/Pulse/PipeWire depending on platform).
Both MVPs validate the same core engine. If both play audio, the
platform-agnostic core is ready to extract into the cascadia-audio crate.

## Next
If audio plays on both Android and Windows: extract shared core into
`cascadiacollections/cascadia-audio` workspace crate, add AudioHandle
state machine, wire UniFFI for Kotlin/Swift and P/Invoke C ABI as
separate crate features.

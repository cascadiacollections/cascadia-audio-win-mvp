using System.Runtime.InteropServices;

namespace CascadiaAudioMvp.Services;

internal static partial class NativeAudio
{
    [LibraryImport("cascadia_audio_win_mvp", EntryPoint = "cascadia_audio_start", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int CascadiaAudioStart(string url);

    [LibraryImport("cascadia_audio_win_mvp", EntryPoint = "cascadia_audio_stop")]
    internal static partial int CascadiaAudioStop();

    [LibraryImport("cascadia_audio_win_mvp", EntryPoint = "cascadia_audio_is_playing")]
    internal static partial int CascadiaAudioIsPlaying();

    [LibraryImport("cascadia_audio_win_mvp", EntryPoint = "cascadia_audio_state")]
    internal static partial int CascadiaAudioState();

    [LibraryImport("cascadia_audio_win_mvp", EntryPoint = "cascadia_audio_last_error")]
    internal static partial int CascadiaAudioLastError(byte[] buffer, nuint bufferLen);
}

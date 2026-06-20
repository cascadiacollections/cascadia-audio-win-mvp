using System.Runtime.InteropServices;

namespace CascadiaAudioMvp.Services;

internal static partial class NativeAudio
{
    [LibraryImport("cascadia_audio_win_mvp", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int CascadiaAudioStart(string url);

    [LibraryImport("cascadia_audio_win_mvp")]
    internal static partial int CascadiaAudioStop();

    [LibraryImport("cascadia_audio_win_mvp")]
    internal static partial int CascadiaAudioIsPlaying();
}


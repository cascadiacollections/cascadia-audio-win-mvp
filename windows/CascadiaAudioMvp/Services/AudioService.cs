namespace CascadiaAudioMvp.Services;

public sealed class AudioService
{
    private const string TestStream = "https://kexp.streamguys1.com/kexp128.mp3";

    public bool IsPlaying => NativeAudio.CascadiaAudioIsPlaying() == 1;

    public bool Play(string? url = null) =>
        NativeAudio.CascadiaAudioStart(url ?? TestStream) == 1;

    public void Stop() =>
        NativeAudio.CascadiaAudioStop();
}

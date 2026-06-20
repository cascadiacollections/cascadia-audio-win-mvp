namespace CascadiaAudioMvp.Services;

public sealed class AudioService
{
    private const string TestStream = "https://kexp.streamguys1.com/kexp64.aac";
    private readonly Func<string, int> _start;
    private readonly Func<int> _stop;
    private readonly Func<int> _isPlaying;

    public AudioService()
        : this(
            NativeAudio.CascadiaAudioStart,
            NativeAudio.CascadiaAudioStop,
            NativeAudio.CascadiaAudioIsPlaying)
    {
    }

    internal AudioService(Func<string, int> start, Func<int> stop, Func<int> isPlaying)
    {
        _start = start;
        _stop = stop;
        _isPlaying = isPlaying;
    }

    public bool IsPlaying => _isPlaying() == 1;

    public bool Play(string? url = null) =>
        _start(url ?? TestStream) == 1;

    public void Stop() =>
        _ = _stop();
}

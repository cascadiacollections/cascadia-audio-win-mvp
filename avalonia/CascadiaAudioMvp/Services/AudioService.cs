using System.Text;

namespace CascadiaAudioMvp.Services;

public enum PlaybackState
{
    Stopped = 0,
    Starting = 1,
    Buffering = 2,
    Playing = 3,
    Reconnecting = 4,
    Stopping = 5,
    Error = 6,
}

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
    public PlaybackState State => (PlaybackState)NativeAudio.CascadiaAudioState();

    public bool Play(string? url = null) =>
        _start(url ?? TestStream) == 1;

    public void Stop() =>
        _ = _stop();

    public string LastError
    {
        get
        {
            var buffer = new byte[1024];
            int len = NativeAudio.CascadiaAudioLastError(buffer, (nuint)buffer.Length);
            if (len <= 0) {
                return string.Empty;
            }

            return Encoding.UTF8.GetString(buffer, 0, len);
        }
    }
}

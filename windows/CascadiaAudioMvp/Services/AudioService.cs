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
    private const string TestStream = "https://kexp.streamguys1.com/kexp128.mp3";

    public bool IsPlaying => NativeAudio.CascadiaAudioIsPlaying() == 1;
    public PlaybackState State => (PlaybackState)NativeAudio.CascadiaAudioState();

    public bool Play(string? url = null) =>
        NativeAudio.CascadiaAudioStart(url ?? TestStream) == 1;

    public void Stop() =>
        NativeAudio.CascadiaAudioStop();

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

using CascadiaAudioMvp.Services;
using Xunit;

namespace CascadiaAudioMvp.Tests;

public sealed class AudioServiceTests
{
    [Fact]
    public void IsPlaying_MapsNativeOneToTrue()
    {
        var sut = new AudioService(_ => 1, () => 1, () => 1);
        Assert.True(sut.IsPlaying);
    }

    [Fact]
    public void Play_UsesDefaultStreamWhenUrlMissing()
    {
        string? receivedUrl = null;
        var sut = new AudioService(
            url =>
            {
                receivedUrl = url;
                return 1;
            },
            () => 1,
            () => 0);

        Assert.True(sut.Play());
        Assert.Equal("https://kexp.streamguys1.com/kexp64.aac", receivedUrl);
    }

    [Fact]
    public void Play_UsesProvidedStreamUrl()
    {
        string? receivedUrl = null;
        var sut = new AudioService(
            url =>
            {
                receivedUrl = url;
                return 1;
            },
            () => 1,
            () => 0);

        Assert.True(sut.Play("https://example.test/live.mp3"));
        Assert.Equal("https://example.test/live.mp3", receivedUrl);
    }

    [Fact]
    public void Stop_InvokesNativeStop()
    {
        var stopCalls = 0;
        var sut = new AudioService(_ => 1, () => ++stopCalls, () => 0);

        sut.Stop();

        Assert.Equal(1, stopCalls);
    }
}

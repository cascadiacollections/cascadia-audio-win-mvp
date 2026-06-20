using Avalonia.Controls;
using Avalonia.Interactivity;
using Avalonia.Threading;
using CascadiaAudioMvp.Services;

namespace CascadiaAudioMvp;

public partial class MainWindow : Window
{
    private readonly AudioService _audio;
    private readonly DispatcherTimer _stateTimer;

    public MainWindow()
    {
        InitializeComponent();
        _audio = new AudioService();
        _stateTimer = new DispatcherTimer
        {
            Interval = TimeSpan.FromMilliseconds(500),
        };
        _stateTimer.Tick += (_, _) => RefreshPlaybackState();
        _stateTimer.Start();
    }

    private void PlayButton_Click(object? sender, RoutedEventArgs e)
    {
        bool ok = _audio.Play();
        if (!ok)
        {
            StatusText.Text = "Failed to start stream";
            return;
        }

        RefreshPlaybackState();
    }

    private void StopButton_Click(object? sender, RoutedEventArgs e)
    {
        _audio.Stop();
        RefreshPlaybackState();
    }

    private void RefreshPlaybackState()
    {
        switch (_audio.State)
        {
            case PlaybackState.Starting:
                StatusText.Text = "Starting stream...";
                PlayButton.IsEnabled = false;
                StopButton.IsEnabled = true;
                break;
            case PlaybackState.Buffering:
                StatusText.Text = "Buffering...";
                PlayButton.IsEnabled = false;
                StopButton.IsEnabled = true;
                break;
            case PlaybackState.Playing:
                StatusText.Text = "Playing...";
                PlayButton.IsEnabled = false;
                StopButton.IsEnabled = true;
                break;
            case PlaybackState.Reconnecting:
                StatusText.Text = "Reconnecting stream...";
                PlayButton.IsEnabled = false;
                StopButton.IsEnabled = true;
                break;
            case PlaybackState.Error:
                var error = _audio.LastError;
                StatusText.Text = string.IsNullOrWhiteSpace(error)
                    ? "Playback failed"
                    : $"Playback failed: {error}";
                PlayButton.IsEnabled = true;
                StopButton.IsEnabled = false;
                break;
            default:
                StatusText.Text = "Stopped";
                PlayButton.IsEnabled = true;
                StopButton.IsEnabled = false;
                break;
        }
    }
}

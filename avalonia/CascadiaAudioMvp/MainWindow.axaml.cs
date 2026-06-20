using Avalonia.Controls;
using Avalonia.Interactivity;
using CascadiaAudioMvp.Services;

namespace CascadiaAudioMvp;

public partial class MainWindow : Window
{
    private readonly AudioService _audio;

    public MainWindow()
    {
        InitializeComponent();
        _audio = new AudioService();
    }

    private void PlayButton_Click(object? sender, RoutedEventArgs e)
    {
        bool ok = _audio.Play();
        StatusText.Text = ok ? "Playing..." : "Failed to start stream";
        PlayButton.IsEnabled = false;
        StopButton.IsEnabled = true;
    }

    private void StopButton_Click(object? sender, RoutedEventArgs e)
    {
        _audio.Stop();
        StatusText.Text = "Stopped";
        PlayButton.IsEnabled = true;
        StopButton.IsEnabled = false;
    }
}


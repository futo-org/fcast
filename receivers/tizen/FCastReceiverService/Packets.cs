using System.Collections.Generic;
using System.Text.Json.Serialization;

public enum Opcode
{
    None = 0,
    Play,
    Pause,
    Resume,
    Stop,
    Seek,
    PlaybackUpdate,
    VolumeUpdate,
    SetVolume,
    PlaybackError,
    SetSpeed,
    Version,
    Ping,
    Pong
}

public class PlayMessage
{
    [JsonPropertyName("container")]
    public string Container { get; set; }

    [JsonPropertyName("url")]
    public string Url { get; set; }

    [JsonPropertyName("content")]
    public string Content { get; set; }

    [JsonPropertyName("time")]
    public double? Time { get; set; }

    [JsonPropertyName("speed")]
    public double? Speed { get; set; }

    [JsonPropertyName("headers")]
    public Dictionary<string, string> Headers { get; set; }
}

public class SeekMessage
{
    [JsonPropertyName("time")]
    public double Time { get; set; }
}

public class PlaybackUpdateMessage
{
    [JsonPropertyName("generationTime")]
    public double GenerationTime { get; set; }

    [JsonPropertyName("time")]
    public double Time { get; set; }

    [JsonPropertyName("duration")]
    public double Duration { get; set; }

    [JsonPropertyName("speed")]
    public double Speed { get; set; }

    [JsonPropertyName("state")]
    public int State { get; set; } // 0 = None, 1 = Playing, 2 = Paused
}

public class PlaybackErrorMessage
{
    [JsonPropertyName("message")]
    public string Message { get; set; }
}

public class VolumeUpdateMessage
{
    [JsonPropertyName("generationTime")]
    public double GenerationTime { get; set; }

    [JsonPropertyName("volume")]
    public double Volume { get; set; } // (0-1)
}

public class SetVolumeMessage
{
    [JsonPropertyName("volume")]
    public double Volume { get; set; }
}

public class SetSpeedMessage
{
    [JsonPropertyName("speed")]
    public double Speed { get; set; }
}

public class VersionMessage
{
    [JsonPropertyName("version")]
    public double Version { get; set; }
}

using System;
using System.Text.Json.Serialization;

public class PlayMessage
{
    [JsonPropertyName("container")]
    public required string Container { get; set; }

    [JsonPropertyName("url")]
    public string? Url { get; set; }

    [JsonPropertyName("content")]
    public string? Content { get; set; }

    [JsonPropertyName("time")]
    public double? Time { get; set; }

    [JsonPropertyName("speed")]
    public double? Speed { get; set; }

    [JsonPropertyName("headers")]
    public Dictionary<string, string>? Headers { get; set; }
}

public class SeekMessage
{
    [JsonPropertyName("time")]
    public required double Time { get; set; }
}

public class PlaybackUpdateMessage
{
    [JsonPropertyName("time")]
    public required double Time { get; set; }

    [JsonPropertyName("duration")]
    public required double Duration { get; set; }

    [JsonPropertyName("speed")]
    public required double Speed { get; set; }

    [JsonPropertyName("state")]
    public required int State { get; set; } // 0 = None, 1 = Playing, 2 = Paused
}

public class VolumeUpdateMessage
{
    [JsonPropertyName("volume")]
    public required double Volume { get; set; } // (0-1)
}

public class SetVolumeMessage
{
    [JsonPropertyName("volume")]
    public required double Volume { get; set; }
}

public class SetSpeedMessage
{
    [JsonPropertyName("speed")]
    public required double Speed { get; set; }
}

public class PlaybackErrorMessage
{
    [JsonPropertyName("message")]
    public required string Message { get; set; }
}

public class VersionMessage
{
    [JsonPropertyName("version")]
    public required ulong Version { get; set; }
}

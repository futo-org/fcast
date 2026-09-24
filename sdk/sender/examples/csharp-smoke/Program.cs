using FCast.SenderSDK;

class Logger : LogHandler
{
    public void Log(LogLevel level, string tag, string message)
    {
        if (level <= LogLevel.Warn || Environment.GetEnvironmentVariable("SMOKE_DEBUG") != null) Console.WriteLine($"[sdk {level}] {tag}: {message}");
        Interlocked.Increment(ref Program.LogLines);
    }
}

class Discovery : DeviceDiscovererEventHandler
{
    public void DeviceAvailable(DeviceInfo d)
    {
        var addrs = string.Join("/", d.Addresses.Select(FcastSenderSdkMethods.UrlFormatIpAddr));
        lock (Program.Seen) Program.Seen.Add($"{d.Name} {d.Protocol} {addrs}:{d.Port} txt={d.TxtRecords.Count}");
        if (d.Name == Program.Target && d.Protocol == ProtocolType.FCast) Program.Set(() => Program.Found = d);
    }
    public void DeviceChanged(DeviceInfo d) {}
    public void DeviceRemoved(string n) {}
}

class Events : DeviceEventHandler
{
    public void ConnectionStateChanged(DeviceConnectionState s) { Program.Set(() => Program.Conn = s); }
    public void VolumeChanged(double v) { Program.Set(() => Program.Volume = v); }
    public void TimeChanged(double t) { Program.Set(() => Program.Time = t); }
    public void PlaybackStateChanged(PlaybackState s) { Program.Set(() => Program.State = s); }
    public void DurationChanged(double d) { Program.Set(() => Program.Duration = d); }
    public void SpeedChanged(double s) { Program.Set(() => Program.Speed = s); }
    public void SourceChanged(Source s) { Program.Set(() => Program.Src = s); }
    public void PlaybackStopped() { Program.Set(() => Program.Stopped = true); }
    public void PlaybackError(string m) { Console.WriteLine($"playback error: {m}"); }
    public void TracksAvailable(MediaTrack[] t) { Program.Set(() => Program.Tracks = t.Length); }
    public void TrackSelected(uint? id, MediaTrackType typ) {}
    public void TracksChanged(TrackList t) {}
    public void QueueChanged(QueueState q) {}
    public void CommandError(ReceiverError e) { Console.WriteLine($"command error: {e}"); }
}

public class Program
{
    public static readonly object L = new();
    public static List<string> Seen = new();
    public static string Target = "";
    public static DeviceInfo? Found;
    public static int LogLines;
    public static DeviceConnectionState? Conn;
    public static PlaybackState? State;
    public static double Time = -1, Duration = -1, Volume = -1, Speed = -1;
    public static Source? Src;
    public static bool Stopped;
    public static int Tracks = -1;

    public static string Dump() => $"conn={(Conn is DeviceConnectionState.Connected ? "Connected" : Conn?.ToString())} state={State} time={Time:F2} dur={Duration:F2} vol={Volume} speed={Speed} src={Src != null}";

    public static void Set(Action a) { lock (L) { a(); Monitor.PulseAll(L); } }

    static void Step(string name, int timeoutMs, Func<bool> done)
    {
        var deadline = DateTime.UtcNow.AddMilliseconds(timeoutMs);
        lock (L)
        {
            while (!done())
            {
                var left = deadline - DateTime.UtcNow;
                if (left <= TimeSpan.Zero)
                {
                    lock (Seen) Console.WriteLine($"discovered: {string.Join(", ", Seen)}");
                    Console.WriteLine($"FAIL {name} ({Dump()})");
                    Environment.Exit(1);
                }
                Monitor.Wait(L, left);
            }
        }
        Console.WriteLine($"ok   {name} ({Dump()})");
    }

    public static void Main(string[] args)
    {
        var url = args[0];
        Target = args.Length > 1 ? args[1] : "";
        FcastSenderSdkMethods.InitCustomLogger(new Logger());
        var ctx = new CastContext();
        ctx.StartDiscovery(new Discovery());

        Step($"discover '{Target}'", 10000, () => Found != null);
        var dev = ctx.CreateDeviceFromInfo(Found!);
        dev.Connect(new ApplicationInfo("csharp-smoke", "0.0.1", "C# smoke"), new Events(), 1000);
        Step("connect", 10000, () => Conn is DeviceConnectionState.Connected);
        if (Conn is DeviceConnectionState.Connected c && c.Capabilities?.Media is MediaCapabilities m)
        {
            Console.WriteLine($"     protocols=[{string.Join(",", m.Protocols)}] containers=[{string.Join(",", m.Containers)}]");
            Console.WriteLine($"     video=[{string.Join(",", m.VideoFormats)}] audio=[{string.Join(",", m.AudioFormats)}] subs=[{string.Join(",", m.SubtitleFormats)}]");
            Console.WriteLine($"     hdr=[{string.Join(",", m.HdrFormats)}] images=[{string.Join(",", m.ImageFormats)}] extsubs={m.ExternalSubtitles} mirroring={m.Mirroring}");
            Console.WriteLine($"     display={c.Capabilities.Display?.Resolution} volume step={c.Capabilities.Audio?.VolumeStepInterval}");
        }
        var features = Enum.GetValues<DeviceFeature>().Where(dev.SupportsFeature);
        Console.WriteLine($"     features=[{string.Join(",", features)}]");
        dev.ChangeVolume(0.0);
        Step("mute", 5000, () => Volume == 0.0);

        dev.Load(new LoadRequest.Url("video/x-matroska", url, 0.0, null, 0.0, new Metadata("smoke", null), null), 250);
        Step("load -> playing", 20000, () => State == PlaybackState.Playing && Time > 1.0);
        Step("duration", 5000, () => Duration > 0);
        // v4 never reports the originator's own load as SourceChanged, only the initial state on connect does

        dev.Seek(20.0);
        Step("seek", 10000, () => State == PlaybackState.Playing && Time >= 20 && Time < 23);
        dev.PausePlayback();
        Step("pause", 5000, () => State == PlaybackState.Paused);
        double pausedAt;
        lock (L) pausedAt = Time;
        dev.ResumePlayback();
        Step("resume", 5000, () => State == PlaybackState.Playing && Time > pausedAt + 0.5);
        dev.ChangeVolume(0.01);
        Step("volume", 5000, () => Math.Abs(Volume - 0.01) < 0.001);
        dev.ChangeSpeed(1.5);
        Step("speed", 5000, () => Math.Abs(Speed - 1.5) < 0.01);

        dev.StopPlayback();
        Step("stop", 5000, () => Stopped || State == PlaybackState.Idle);
        dev.Disconnect();
        Step("disconnect", 5000, () => Conn is DeviceConnectionState.Disconnected);

        lock (Seen) Console.WriteLine($"discovered: {string.Join(", ", Seen)}");
        Console.WriteLine($"tracks={Tracks} sdk log lines={LogLines} src={Src}");
        Console.WriteLine("PASS");
    }
}

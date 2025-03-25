using System.Collections.Generic;
using System.Text.Json;
using System.Threading.Tasks;
using Serilog;
using Tizen.Applications;
using Tizen.Applications.Messages;
using System;
using Tizen.Network.Nsd;

namespace FCastReceiverService
{
    internal class Program : ServiceApplication
    {
        private const string AppId = "ql5ofothoj.fcastreceiver";
        private const string AppPort = "ipcPort";
        private static AppControl _appControl;
        private static RemotePort _appPort;
        public static MessagePort IpcPort { get; private set; }

        private static DnssdService tcpDnssdService;
        private static TcpListenerService tcpListenerService;
        private static WebSocketListnerService webSocketListenerService;

        public static bool websocketsSupported = false;

        protected override void OnCreate()
        {
            base.OnCreate();

            Serilog.Log.Information($"Starting: {Program.Current.ApplicationInfo.ApplicationId}");
            Serilog.Log.Information($"Version: 1.0.0");
            Serilog.Log.Information($"Manufacturer: {SystemInformation.Manufacturer}");
            Serilog.Log.Information($"ModelName: {SystemInformation.ModelName}");
            Serilog.Log.Information($"PlatformName: {SystemInformation.PlatformName}");

            Serilog.Log.Information($"BuildDate: {SystemInformation.BuildDate}");
            Serilog.Log.Information($"BuildId: {SystemInformation.BuildId}");
            Serilog.Log.Information($"BuildRelease: {SystemInformation.BuildRelease}");
            Serilog.Log.Information($"BuildString: {SystemInformation.BuildString}");

            _appControl = new AppControl();
            _appControl.ApplicationId = AppId;

            _appPort = new RemotePort(AppId, AppPort, false);
            _appPort.RemotePortStateChanged += RemotePortStateChanged;

            IpcPort = new MessagePort(AppPort, false);
            IpcPort.MessageReceived += IpcMainMessageCb;
            IpcPort.Listen();
            SendAppMessage("serviceStart");


            // Note: Unable to find required shared library when running in emulator...
            tcpDnssdService = new DnssdService("_fcast._tcp");
            tcpDnssdService.Port = TcpListenerService.Port;
            tcpDnssdService.Name = $"{SystemInformation.Manufacturer} {SystemInformation.ModelName}";
            tcpDnssdService.RegisterService();

            tcpListenerService = new TcpListenerService();
            List<IListenerService> listeners = new List<IListenerService>() { tcpListenerService };

            // Older Tizen models seem to throw exceptions when accessing standard .NET APIs for
            // Querying network interface information or using HttpListeners...
            // May need to investigate further however, perhaps its only an issue when running in emulator

            // No API to get Tizen version, so have to go by OS image build date...
            // Format: YYYYMMDD_HHMMSS
            if (int.Parse(SystemInformation.BuildDate.Substring(0, 4)) > 2018)
            {
                websocketsSupported = true;
                webSocketListenerService = new WebSocketListnerService();
                listeners.Add(webSocketListenerService);
            }

            foreach (IListenerService l in listeners)
            {
                l.OnPlay += Program.OnPlay;
                l.OnPause += (object sender, EventArgs e) => { SendAppMessage("pause"); };
                l.OnResume += (object sender, EventArgs e) => { SendAppMessage("resume"); };
                l.OnStop += (object sender, EventArgs e) => { SendAppMessage("stop"); };
                l.OnSeek += (object sender, SeekMessage e) =>
                {
                    SendAppMessage("seek", JsonSerializer.Serialize(e));
                };
                l.OnSetVolume += (object sender, SetVolumeMessage e) =>
                {
                    SendAppMessage("setvolume", JsonSerializer.Serialize(e));
                };
                l.OnSetSpeed += (object sender, SetSpeedMessage e) =>
                {
                    SendAppMessage("setspeed", JsonSerializer.Serialize(e));
                };
                l.OnPing += (object sender, Dictionary<string, string> e) => { SendAppMessage("ping", e); };

                l.OnConnect += (object sender, Dictionary<string, string> e) => { SendAppMessage("connect", e); };
                l.OnDisconnect += (object sender, Dictionary<string, string> e) => { SendAppMessage("disconnect", e); };

                l.ListenAsync();
            }

            SendAppMessage("serviceStarted", new Dictionary<string, string>() {
                { "websocketsSupported", websocketsSupported.ToString() },
                { "buildDate", SystemInformation.BuildDate },
                { "buildId", SystemInformation.BuildId },
                { "buildRelease", SystemInformation.BuildRelease },
                { "buildString", SystemInformation.BuildString },
            });
        }

        protected override void OnTerminate()
        {
            SendAppMessage("serviceExit");

            tcpDnssdService.DeregisterService();
            tcpDnssdService.Dispose();

            base.OnTerminate();
        }

        private static void OnPlay(object sender, PlayMessage e)
        {
            if (!ApplicationManager.IsRunning(AppId))
            {
                Serilog.Log.Information("FCast application not running, launching application");
                AppControl.SendLaunchRequest(_appControl);
                ReattemptOnPlay(sender, e);
                return;
            }
            else
            {
                ApplicationRunningContext appContext = new ApplicationRunningContext(AppId);
                if (appContext.State == ApplicationRunningContext.AppState.Background)
                {
                    Serilog.Log.Information("FCast application suspended, resuming");
                    appContext.Resume();
                    ReattemptOnPlay(sender, e);
                    return;
                }
            }

            e = NetworkService.ProxyPlayIfRequired(e);
            Serilog.Log.Information($"Sending play message: {e}");

            SendAppMessage("play", JsonSerializer.Serialize(e));
        }

        private static void ReattemptOnPlay(object sender, PlayMessage e)
        {
            Task.Run(async () =>
            {
                int delay = 1000;
                while (true)
                {
                    // Drop play message after ~20s if app does not startup or resume to foreground
                    if (delay > 6000)
                    {
                        return;
                    }

                    if (ApplicationManager.IsRunning(AppId))
                    {
                        ApplicationRunningContext appContext = new ApplicationRunningContext(AppId);
                        if (appContext.State == ApplicationRunningContext.AppState.Foreground)
                        {
                            OnPlay(sender, e);
                            return;
                        }
                    }

                    Serilog.Log.Information($"Waiting {delay}ms for application to start");
                    await Task.Delay(delay);
                    delay += 1000;
                }
            });
        }

        public static void SendAppMessage(string message, Dictionary<string, string> data)
        {
            SendAppMessage(message, JsonSerializer.Serialize(data));
        }

        public static void SendAppMessage(string message, string data = "null")
        {
            if (_appPort.IsRunning())
            {
                Bundle bundle = new Bundle();
                bundle.AddItem("message", message);
                bundle.AddItem("data", data);

                IpcPort.Send(bundle, AppId, AppPort);
            }
            else
            {
                Serilog.Log.Warning($"App is currently not running, cannot send message: {message} {data}");
            }
        }

        private static void RemotePortStateChanged(object sender, RemotePortStateChangedEventArgs e)
        {
            switch (e.Status)
            {
                case State.Registered:
                    Serilog.Log.Information("Remote ipc port is registered");
                    break;
                case State.Unregistered:
                    Serilog.Log.Information("Remote ipc port is unregistered");
                    break;
                default:
                    break;
            }
        }

        private static void IpcMainMessageCb(object sender, MessageReceivedEventArgs e)
        {
            Serilog.Log.Information($"Message received in main handler with {e.Message.Count} items");
            e.Message.TryGetItem("command", out string command);

            switch (command)
            {
                case "getSystemInfo":
                    SendAppMessage("getSystemInfo", new Dictionary<string, string>() {
                        { "websocketsSupported", websocketsSupported.ToString() },
                        { "buildDate", SystemInformation.BuildDate },
                        { "buildId", SystemInformation.BuildId },
                        { "buildRelease", SystemInformation.BuildRelease },
                        { "buildString", SystemInformation.BuildString },
                    });
                    break;

                default:
                    Serilog.Log.Information($"Unknown message with command {command}");
                    break;
            }
        }

        public static void DebugAppMessage(string message, int icon = 0)
        {
            SendAppMessage("toast", new Dictionary<string, string>() { { "message", message }, { "icon", icon.ToString() } });
        }

        static void Main(string[] args)
        {
            try
            {
                Serilog.Log.Logger = new Serilog.LoggerConfiguration().WriteTo.Debug().CreateLogger();
                var app = new Program();
                app.Run(args);
            }
            catch (Exception e)
            {
                Serilog.Log.Error($"Network service: {e}");
                DebugAppMessage($"Network service: {e}", 1);
            }
        }
    }
}

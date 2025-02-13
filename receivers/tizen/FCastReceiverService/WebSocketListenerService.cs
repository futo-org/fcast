using System;
using System.Collections.Generic;
using System.Net;
using System.Net.Sockets;
using System.Net.WebSockets;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using Tizen.Applications.Messages;

namespace FCastReceiverService
{
    public class WebSocketListnerService : IListenerService, IDisposable
    {
        public const int Port = 46898;
        private HttpListener _listener;
        private List<FCastSession> _sessions;
        private CancellationTokenSource _cancellationTokenSource;

        public event EventHandler<PlayMessage> OnPlay;
        public event EventHandler OnPause;
        public event EventHandler OnResume;
        public event EventHandler OnStop;
        public event EventHandler<SeekMessage> OnSeek;
        public event EventHandler<SetVolumeMessage> OnSetVolume;
        public event EventHandler<SetSpeedMessage> OnSetSpeed;
        public event EventHandler<VersionMessage> OnVersion;
        public event EventHandler<Dictionary<string, string>> OnPing;
        public event EventHandler OnPong;

        public event EventHandler<Dictionary<string, string>> OnConnect;
        public event EventHandler<Dictionary<string, string>> OnDisconnect;

        public WebSocketListnerService()
        {
            _sessions = new List<FCastSession>();
            _cancellationTokenSource = new CancellationTokenSource();

            _listener = new HttpListener();
            foreach (IPAddress address in NetworkService.GetAllIPAddresses())
            {
                Serilog.Log.Information($"Adding WS listener address: {address}");
                _listener.Prefixes.Add($"http://{address}:{Port}/");
            }

            _listener.Start();
        }

        public async Task ListenAsync()
        {
            Serilog.Log.Information("Listening for WS connections...");
            while (!_cancellationTokenSource.IsCancellationRequested)
            {
                HttpListenerContext context = await _listener.GetContextAsync();
                if (!context.Request.IsWebSocketRequest)
                {
                    context.Response.StatusCode = 400;
                    context.Response.Close();
                    continue;
                }

                HttpListenerWebSocketContext webSocketContext = await context.AcceptWebSocketAsync(null);
                Serilog.Log.Information($"New WS connection from {webSocketContext.Origin}");

                FCastSession session = new FCastSession(new WebSocketStream(webSocketContext.WebSocket));
                Guid connectionId = Guid.NewGuid();
                
                session.OnPlay += OnPlay;
                session.OnPause += OnPause;
                session.OnResume += OnResume;
                session.OnStop += OnStop;
                session.OnSeek += OnSeek;
                session.OnSetVolume += OnSetVolume;
                session.OnSetSpeed += OnSetSpeed;
                session.OnVersion += OnVersion;
                session.OnPing += (object sender, EventArgs e) =>
                {
                    OnPing?.Invoke(this, new Dictionary<string, string>() { { "id", connectionId.ToString() } });
                };
                session.OnPong += OnPong;
                _sessions.Add(session);

                EventHandler<MessageReceivedEventArgs> ipcMessageCb = (object sender, MessageReceivedEventArgs e) =>
                {
                    Serilog.Log.Information($"Message received in websockets handler with {e.Message.Count} items");
                    e.Message.TryGetItem("opcode", out string opcode);
                    Enum.TryParse(opcode, out Opcode code);
                    e.Message.TryGetItem("data", out string data);

                    switch (code)
                    {
                        case Opcode.PlaybackError:
                            _ = session.SendMessageAsync(code, JsonSerializer.Deserialize<PlaybackErrorMessage>(data), _cancellationTokenSource.Token);
                            break;

                        case Opcode.PlaybackUpdate:
                            _ = session.SendMessageAsync(code, JsonSerializer.Deserialize<PlaybackUpdateMessage>(data), _cancellationTokenSource.Token);
                            break;

                        case Opcode.VolumeUpdate:
                            _ = session.SendMessageAsync(code, JsonSerializer.Deserialize<VolumeUpdateMessage>(data), _cancellationTokenSource.Token);
                            break;

                        default:
                            Serilog.Log.Information($"Unknown message with opcode {code} and data {data}");
                            break;
                    }
                };
                Program.IpcPort.MessageReceived += ipcMessageCb;

                session.OnDispose += (object sender, EventArgs e) =>
                {
                    _sessions.Remove(session);
                    Program.IpcPort.MessageReceived -= ipcMessageCb;

                    OnDisconnect?.Invoke(this, new Dictionary<string, string>() {
                        { "id", connectionId.ToString() },
                        { "type", "ws" },
                        { "data", JsonSerializer.Serialize(new Dictionary<string, string>() { { "url", webSocketContext.Origin } }) }
                    });
                };

                OnConnect?.Invoke(this, new Dictionary<string, string>() {
                    { "id", connectionId.ToString() },
                    { "type", "ws" },
                    { "data", JsonSerializer.Serialize(new Dictionary<string, string>() { { "url", webSocketContext.Origin } }) }
                });

                Serilog.Log.Information("Sending version");
                _ = session.SendMessageAsync(Opcode.Version, new VersionMessage() { Version = 2, }, _cancellationTokenSource.Token);
                _ = SessionListenAsync(webSocketContext, session);

            }
        }

        private async Task SessionListenAsync(HttpListenerWebSocketContext context, FCastSession session)
        {
            try
            {
                await session.ReceiveLoopAsync(_cancellationTokenSource.Token);
            }
            catch (SocketException e)
            {
                Serilog.Log.Error($"Socket error from {context.Origin}: {e}");
            }
            finally
            {
                session.Dispose();
            }
        }

        public void Dispose()
        {
            _listener.Stop();
        }
    }
}

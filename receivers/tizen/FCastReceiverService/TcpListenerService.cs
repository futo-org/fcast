using System;
using System.Collections.Generic;
using System.Net.Sockets;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using Tizen.Applications.Messages;

namespace FCastReceiverService
{
    public class TcpListenerService : IDisposable
    {
        public const int Port = 46899;
        private const int Timeout = 2500;
        private TcpListener _listener;
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

        public TcpListenerService()
        {
            _sessions = new List<FCastSession>();
            _cancellationTokenSource = new CancellationTokenSource();

            _listener = new TcpListener(Port);
            _listener.Start();
        }

        public async Task ListenAsync()
        {
            Serilog.Log.Information("Listening for TCP connections...");
            while (!_cancellationTokenSource.IsCancellationRequested)
            {
                //TcpClient client = await _listener.AcceptTcpClientAsync(_cancellationTokenSource.Token);
                TcpClient client = await _listener.AcceptTcpClientAsync();
                Serilog.Log.Information($"New TCP connection from {client.Client.RemoteEndPoint}");

                client.ReceiveTimeout = Timeout;
                NetworkStream stream = client.GetStream();

                FCastSession session = new FCastSession(stream);
                Guid connectionId = Guid.NewGuid();
                int heartbeatRetries = 0;

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
                    Serilog.Log.Information($"Message received in tcp handler with {e.Message.Count} items");
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

                session.OnTimeout += (object sender, EventArgs e) =>
                {
                    try
                    {
                        if (heartbeatRetries > 3)
                        {
                            Serilog.Log.Warning($"Could not ping device {client.Client.RemoteEndPoint}. Disconnecting...");
                            session.Dispose();
                        }

                        heartbeatRetries += 1;
                        _ = session.SendMessageAsync(Opcode.Ping, _cancellationTokenSource.Token);
                    }
                    catch
                    {
                        Serilog.Log.Warning($"Error while pinging sender device {client.Client.RemoteEndPoint}.");
                        session.Dispose();
                    }
                };
                session.OnData += (object sender, EventArgs e) => { heartbeatRetries = 0; };
                session.OnDispose += (object sender, EventArgs e) =>
                {
                    _sessions.Remove(session);
                    Program.IpcPort.MessageReceived -= ipcMessageCb;

                    OnDisconnect?.Invoke(this, new Dictionary<string, string>() {
                        { "id", connectionId.ToString() },
                        { "type", "tcp" },
                        { "data", JsonSerializer.Serialize(new Dictionary<string, string>() { { "address", client.Client.RemoteEndPoint.ToString() } }) }
                    });
                };

                OnConnect?.Invoke(this, new Dictionary<string, string>() {
                    { "id", connectionId.ToString() },
                    { "type", "tcp" },
                    { "data", JsonSerializer.Serialize(new Dictionary<string, string>() { { "address", client.Client.RemoteEndPoint.ToString() } }) }
                });

                // Program.DebugAppMessage("TESTING MESSAGE FOR BINDINGS");
                Serilog.Log.Information("Sending version");
                _ = session.SendMessageAsync(Opcode.Version, new VersionMessage() { Version = 2, }, _cancellationTokenSource.Token);
                _ = SessionListenAsync(client, session);

            }
        }

        private async Task SessionListenAsync(TcpClient client, FCastSession session)
        {
            try
            {
                await session.ReceiveLoopAsync(_cancellationTokenSource.Token);
            }
            catch (SocketException e)
            {
                Serilog.Log.Error($"Socket error from {client.Client.RemoteEndPoint}: {e}");
            }
            finally
            {
                session.Dispose();
                client.Dispose();
            }
        }

        public void Dispose()
        {
            _listener.Stop();
        }
    }
}

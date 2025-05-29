using System.ComponentModel;
using System.Linq;
using System.Net;
using System.Net.Sockets;
using System.Net.WebSockets;
using FCastSender;
using NestedArgs;

internal class Program
{
    private static async Task Main(string[] args)
    {
        Command rootCommand = new CommandBuilder("fcast", "Control FCast Receiver through the terminal.")
            .Option(new Option()
            {
                LongName = "connection_type",
                ShortName = 'c',
                Description = "Type of connection: tcp or ws (websocket)",
                DefaultValue = "tcp",
                IsRequired = false
            })
            .Option(new Option()
            {
                LongName = "host",
                ShortName = 'h',
                Description = "The host address to send the command to",
                IsRequired = true
            })
            .Option(new Option()
            {
                LongName = "port",
                ShortName = 'p',
                Description = "The port to send the command to",
                IsRequired = false
            })
            .SubCommand(new CommandBuilder("play", "Play media")
                .Option(new Option()
                {
                    LongName = "mime_type",
                    ShortName = 'm',
                    Description = "Mime type (e.g., video/mp4)",
                    IsRequired = true
                })
                .Option(new Option()
                {
                    LongName = "file",
                    ShortName = 'f',
                    Description = "File content to play",
                    IsRequired = false
                })
                .Option(new Option()
                {
                    LongName = "url",
                    ShortName = 'u',
                    Description = "URL to the content",
                    IsRequired = false
                })
                .Option(new Option()
                {
                    LongName = "content",
                    ShortName = 'c',
                    Description = "The actual content",
                    IsRequired = false
                })
                .Option(new Option()
                {
                    LongName = "timestamp",
                    ShortName = 't',
                    Description = "Timestamp to start playing",
                    DefaultValue = "0",
                    IsRequired = false
                })
                .Option(new Option()
                {
                    LongName = "speed",
                    ShortName = 's',
                    Description = "Factor to multiply playback speed by",
                    DefaultValue = "1",
                    IsRequired = false
                })
                .Option(new Option()
                {
                    LongName = "header",
                    ShortName = 'H',
                    Description = "HTTP header to add to request",
                    IsRequired = false,
                    AllowMultiple = true
                })
                .Build())
            .SubCommand(new CommandBuilder("seek", "Seek to a timestamp")
                .Option(new Option()
                {
                    LongName = "timestamp",
                    ShortName = 't',
                    Description = "Timestamp to start playing",
                    IsRequired = true
                })
                .Build())
            .SubCommand(new CommandBuilder("pause", "Pause media").Build())
            .SubCommand(new CommandBuilder("resume", "Resume media").Build())
            .SubCommand(new CommandBuilder("stop", "Stop media").Build())
            .SubCommand(new CommandBuilder("listen", "Listen to incoming events").Build())
            .SubCommand(new CommandBuilder("setvolume", "Set the volume")
                .Option(new Option()
                {
                    LongName = "volume",
                    ShortName = 'v',
                    Description = "Volume level (0-1)",
                    IsRequired = true
                })
                .Build())
            .SubCommand(new CommandBuilder("setspeed", "Set the playback speed")
                .Option(new Option()
                {
                    LongName = "speed",
                    ShortName = 's',
                    Description = "Factor to multiply playback speed by",
                    IsRequired = true
                })
                .Build())
            .Build();

        CommandMatches matches = rootCommand.Parse(args).Matches;
        Console.WriteLine(matches.ToString());

        var host = matches.Value("host")!;
        var connectionType = matches.Value("connection_type")!;

        var port = matches.ValueAsInt32("port") ?? connectionType switch
        {
            "tcp" => 46899,
            "ws" => 46898,
            _ => throw new Exception($"{connectionType} is not a valid connection type.")
        };

        var cancellationTokenSource = new CancellationTokenSource();
        var cancellationToken = cancellationTokenSource.Token;
        Console.CancelKeyPress += (_, _) =>
        {
            cancellationTokenSource.Cancel();
        };

        using var session = await EstablishConnection(host, port, connectionType, cancellationToken);
        await session.SendMessageAsync(Opcode.Version, new VersionMessage() { Version = 1 }, cancellationToken);

        switch (matches.SubCommand)
        {
            case "play":
            {
                var playMatches = matches.SubCommandMatch!;
                var mimeType = playMatches.Value("mime_type")!;
                var timestamp = playMatches.ValueAsDouble("timestamp")!;
                var speed = playMatches.ValueAsDouble("speed")!;
                var headers = playMatches.Values("header")?.Select(SplitHeader).ToDictionary(v => v.Key, v => v.Value);

                if (playMatches.Has("file"))
                {
                    IPAddress localAddress;
                    {
                        using var socket = new Socket(AddressFamily.InterNetwork, SocketType.Stream, ProtocolType.Tcp);
                        socket.Connect(host, port);
                        localAddress = (socket.LocalEndPoint as IPEndPoint)!.Address;
                    }

                    var path = playMatches.Value("file")!;
                    var (url, task) = HostFileAndGetUrl(localAddress, path, mimeType, cancellationToken);
                    await session.SendMessageAsync(Opcode.Play, new PlayMessage()
                    {
                        Container = mimeType,
                        Speed = speed,
                        Time = timestamp,
                        Url = url,
                        Headers = headers
                    }, cancellationToken);

                    Console.WriteLine("Waiting for video to finish. Press CTRL+C to exit.");
                    await task;
                }
                else
                {
                    var url = playMatches.Value("url");
                    var content = playMatches.Value("content");

                    await session.SendMessageAsync(Opcode.Play, new PlayMessage()
                    {
                        Container = mimeType,
                        Content = content,
                        Speed = speed,
                        Time = timestamp,
                        Url = url,
                        Headers = headers
                    }, cancellationToken);
                }

                break;
            }
            case "seek":
            {
                await session.SendMessageAsync(Opcode.Seek, new SeekMessage() { Time = matches.SubCommandMatch!.ValueAsDouble("timestamp")!.Value }, cancellationToken);
                break;
            }
            case "pause":
            {
                await session.SendMessageAsync(Opcode.Pause, cancellationToken);
                break;
            }
            case "resume":
            {
                await session.SendMessageAsync(Opcode.Resume, cancellationToken);
                break;
            }
            case "stop":
            {
                await session.SendMessageAsync(Opcode.Stop, cancellationToken);
                break;
            }
            case "listen":
            {
                Console.WriteLine("Listening. Press CTRL+C to exit.");
                await session.ReceiveLoopAsync(cancellationToken);
                break;
            }
            case "setvolume":
            {
                await session.SendMessageAsync(Opcode.SetVolume, new SetVolumeMessage() { Volume = matches.SubCommandMatch!.ValueAsDouble("volume")!.Value }, cancellationToken);
                break;
            }
            case "setspeed":
            {
                await session.SendMessageAsync(Opcode.SetSpeed, new SetSpeedMessage() { Speed = matches.SubCommandMatch!.ValueAsDouble("speed")!.Value }, cancellationToken);
                break;
            }
            default:
                Console.WriteLine("Invalid command. Use --help for more information.");
                break;
        }
    }

    private static int GetAvailablePort()
    {
        using var socket = new Socket(AddressFamily.InterNetwork, SocketType.Stream, ProtocolType.Tcp);
        socket.Bind(new IPEndPoint(IPAddress.Any, 0));
        return ((IPEndPoint)socket.LocalEndPoint!).Port;
    }

    public static (string url, Task serverTask) HostFileAndGetUrl(IPAddress localAddress, string filePath, string mimeType, CancellationToken cancellationToken)
    {
        var listener = new HttpListener();
        listener.Prefixes.Add($"http://{localAddress}:{GetAvailablePort()}/");
        listener.Start();

        var url = listener.Prefixes.First();
        Console.WriteLine($"Server started on {url}.");

        var serverTask = Task.Run(async () =>
        {
            DateTime lastRequestTime = DateTime.Now;
            int activeConnections = 0;

            while (!cancellationToken.IsCancellationRequested)
            {
                if (activeConnections == 0 && (DateTime.Now - lastRequestTime).TotalSeconds > 300)
                {
                    Console.WriteLine("No activity on server, closing...");
                    break;
                }

                if (listener.IsListening)
                {
                    var contextTask = listener.GetContextAsync();
                    await Task.WhenAny(contextTask, Task.Delay(Timeout.Infinite, cancellationToken));

                    if (cancellationToken.IsCancellationRequested)
                        break;

                    var context = contextTask.Result;
                    Console.WriteLine("Request received.");

                    try
                    {
                        Interlocked.Increment(ref activeConnections);
                        lastRequestTime = DateTime.Now;

                        var response = context.Response;
                        response.ContentType = mimeType;
                        using (var fileStream = new FileStream(filePath, FileMode.Open, FileAccess.Read))
                            await fileStream.CopyToAsync(response.OutputStream);
                        response.OutputStream.Close();
                    }
                    catch (Exception ex)
                    {
                        Console.WriteLine($"Error handling request: {ex.Message}");
                    }
                    finally
                    {
                        Interlocked.Decrement(ref activeConnections);
                    }
                }
                else
                {
                    await Task.Delay(5000);
                }
            }

            listener.Stop();
        }, cancellationToken);

        return (url, serverTask);
    }

    public static (string Key, string Value) SplitHeader(string input)
    {
        int colonIndex = input.IndexOf(':');
        if (colonIndex == -1)
        {
            throw new Exception("Header format invalid");
        }

        string beforeColon = input.Substring(0, colonIndex);
        string afterColon = input.Substring(colonIndex + 1);

        return (beforeColon, afterColon);
    }

    private static async Task<FCastSession> EstablishConnection(string host, int port, string connectionType, CancellationToken cancellationToken)
    {
        switch (connectionType.ToLower())
        {
            case "tcp":
                return await EstablishTcpConnection(host, port, cancellationToken);
            case "ws":
                return await EstablishWebSocketConnection(host, port, cancellationToken);
            default:
                throw new ArgumentException("Invalid connection type: " + connectionType);
        }
    }

    private static async Task<FCastSession> EstablishTcpConnection(string host, int port, CancellationToken cancellationToken)
    {
        TcpClient client = new TcpClient();
        await client.ConnectAsync(host, port, cancellationToken);
        return new FCastSession(client.GetStream());
    }

    private static async Task<FCastSession> EstablishWebSocketConnection(string host, int port, CancellationToken cancellationToken)
    {
        ClientWebSocket webSocket = new ClientWebSocket();
        string scheme = "ws";
        string uriString = $"{scheme}://{host}:{port}";
        Uri serverUri = new Uri(uriString);

        await webSocket.ConnectAsync(serverUri, cancellationToken);
        return new FCastSession(new WebSocketStream(webSocket));
    }
}
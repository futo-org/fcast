using System;
using System.Collections.Generic;
using System.Linq;
using System.Net;
using System.Net.Http;
using System.Net.NetworkInformation;
using System.Net.Sockets;
using System.Threading;
using System.Threading.Tasks;


namespace FCastReceiverService
{
    public class NetworkService
    {
        private static HttpListener _proxyServer;
        private static int _proxyServerPort;
        private static string _proxyFileMimeType;
        private static CancellationTokenSource _cancellationTokenSource;

        private static readonly Dictionary<string, (string, Dictionary<string, string>)> _proxiedFiles = new Dictionary<string, (string, Dictionary<string, string>)>();
        private static readonly string[] _streamingMediaTypes = { 
            "application/vnd.apple.mpegurl",
            "application/x-mpegURL",
            "application/dash+xml"
        };        

        private static void SetupProxyServer()
        {
            Serilog.Log.Information("Proxy server starting");
            _proxyServer = new HttpListener();
            _proxyServerPort = GetAvailablePort();
            _cancellationTokenSource = new CancellationTokenSource();

            _proxyServer.Prefixes.Add($"http://127.0.0.1:{GetAvailablePort()}/");
            _proxyServer.Start();
            Serilog.Log.Information($"Proxy server running at http://127.0.0.1:{_proxyServerPort}/");

            var serverTask = Task.Run(async () =>
            {
                DateTime lastRequestTime = DateTime.Now;
                int activeConnections = 0;
                HttpClient client = new HttpClient();

                while (!_cancellationTokenSource.IsCancellationRequested)
                {
                    if (activeConnections == 0 && (DateTime.Now - lastRequestTime).TotalSeconds > 300)
                    {
                        Serilog.Log.Information("No activity on server, closing...");
                        break;
                    }

                    if (_proxyServer.IsListening)
                    {
                        var contextTask = _proxyServer.GetContextAsync();
                        await Task.WhenAny(contextTask, Task.Delay(Timeout.Infinite, _cancellationTokenSource.Token));

                        if (_cancellationTokenSource.IsCancellationRequested)
                            break;

                        var context = contextTask.Result;
                        Serilog.Log.Information("Request received.");

                        // Note: Incomplete implementation, cannot use on Tizen due to sanboxing
                        // blocking requests to localhost between different processes.
                        try
                        {
                            Interlocked.Increment(ref activeConnections);
                            lastRequestTime = DateTime.Now;

                            var request = context.Request;
                            var response = context.Response;
                            string requestUrl = request.Url.ToString();
                            Serilog.Log.Information($"Request URL: {request.Url}");

                            if (!_proxiedFiles.TryGetValue(requestUrl, out var proxyInfo))
                            {
                                response.StatusCode = 404;
                                response.Close();
                                continue;
                            }

                            // TODO Add header custom headers and omitting standard headers

                            //var response = context.Response;
                            response.ContentType = _proxyFileMimeType;

                            var requestStream = await client.GetStreamAsync(proxyInfo.Item1);
                            await requestStream.CopyToAsync(response.OutputStream);

                            //using (var fileStream = new FileStream(filePath, FileMode.Open, FileAccess.Read))
                            //    await fileStream.CopyToAsync(response.OutputStream);
                            response.OutputStream.Close();
                        }
                        catch (Exception ex)
                        {
                            Serilog.Log.Error($"Error handling request: {ex.Message}");
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

                client.Dispose();
                _proxyServer.Stop();
            }, _cancellationTokenSource.Token);
        }

        private static int GetAvailablePort()
        {
            var socket = new Socket(AddressFamily.InterNetwork, SocketType.Stream, ProtocolType.Tcp);
            socket.Bind(new IPEndPoint(IPAddress.Any, 0));
            return ((IPEndPoint)socket.LocalEndPoint).Port;
        }

        public static PlayMessage ProxyPlayIfRequired(PlayMessage message)
        {
            // Disabling file proxying on Tizen, since localhost requests between processes
            // are blocked due to sandboxing...
            //if (message.Headers != null && message.Url != null && !_streamingMediaTypes.Contains(message.Container.ToLower()))
            //{
            //    _proxyFileMimeType = message.Container;
            //    message.Url = ProxyFile(message.Url, message.Headers);
            //}

            return message;
        }
        
        public static string ProxyFile(string url, Dictionary<string, string> headers)
        {
            if (_proxyServer is null)
            {
                SetupProxyServer();
            }
            
            Guid urlId = Guid.NewGuid();
            string proxiedUrl = $"http://127.0.0.1:{_proxyServerPort}/{urlId.ToString()}";
            Serilog.Log.Information($"Proxied url {proxiedUrl} {url} {headers}");
            _proxiedFiles.Add(proxiedUrl, (url, headers));
            return proxiedUrl;
        }

        public static List<IPAddress> GetAllIPAddresses()
        {
            //return Dns.GetHostAddresses(Dns.GetHostName())
            //    .Where(x => IsPrivate(x) && !IPAddress.IsLoopback(x) && x.AddressFamily == AddressFamily.InterNetwork)
            //    .ToList();
            
            return NetworkInterface.GetAllNetworkInterfaces().SelectMany(v => v.GetIPProperties()
                .UnicastAddresses
                .Select(x => x.Address)
                .Where(x => !IPAddress.IsLoopback(x) && x.AddressFamily == AddressFamily.InterNetwork))
                .ToList();
        }

        // https://datatracker.ietf.org/doc/html/rfc1918
        public static bool IsPrivate(IPAddress addr)
        {
            byte[] bytes = addr.GetAddressBytes();
            switch (bytes[0])
            {
                case 10:
                    return true;
                case 172:
                    return bytes[1] < 32 && bytes[1] >= 16;
                case 192:
                    return bytes[1] == 168;
                default:
                    return false;
            }
        }
    }
}

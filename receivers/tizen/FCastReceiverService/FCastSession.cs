using System;
using System.Buffers.Binary;
using System.IO;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;

public enum SessionState
{
    Idle = 0,
    WaitingForLength,
    WaitingForData,
    Disconnected
}

public class FCastSession : IDisposable
{
    private const int LengthBytes = 4;
    private const int MaximumPacketLength = 32000;
    private byte[] _buffer = new byte[MaximumPacketLength];
    private int _bytesRead;
    private int _packetLength;
    private Stream _stream;
    private SemaphoreSlim _writerSemaphore = new SemaphoreSlim(1);
    private SemaphoreSlim _readerSemaphore = new SemaphoreSlim(1);
    private SessionState _state;

    public event EventHandler<PlayMessage> OnPlay;
    public event EventHandler OnPause;
    public event EventHandler OnResume;
    public event EventHandler OnStop;
    public event EventHandler<SeekMessage> OnSeek;
    public event EventHandler<SetVolumeMessage> OnSetVolume;
    public event EventHandler<SetSpeedMessage> OnSetSpeed;
    public event EventHandler<VersionMessage> OnVersion;
    public event EventHandler OnPing;
    public event EventHandler OnPong;

    public event EventHandler OnData;
    public event EventHandler OnTimeout;
    public event EventHandler OnDispose;

    public FCastSession(Stream stream)
    {
        _stream = stream;
        _state = SessionState.Idle;
    }

    public async Task SendMessageAsync(Opcode opcode, CancellationToken cancellationToken)
    {
        await _writerSemaphore.WaitAsync();

        try
        {
            int size = 1;
            byte[] header = new byte[LengthBytes + 1];
            Array.Copy(BitConverter.GetBytes(size), header, LengthBytes);
            header[LengthBytes] = (byte)opcode;

            Serilog.Log.Information($"Sent {header.Length} bytes with (opcode: {opcode}, header size: {header.Length}, no body).");
            await _stream.WriteAsync(header, 0, header.Length, cancellationToken);
        }
        finally
        {
            _writerSemaphore.Release();
        }
    }

    public async Task SendMessageAsync<T>(Opcode opcode, T message, CancellationToken cancellationToken) where T : class
    {
        await _writerSemaphore.WaitAsync();

        try
        {
            string json = JsonSerializer.Serialize(message);
            byte[] data = Encoding.UTF8.GetBytes(json);
            int size = 1 + data.Length;
            byte[] header = new byte[LengthBytes + 1];
            Array.Copy(BitConverter.GetBytes(size), header, LengthBytes);
            header[LengthBytes] = (byte)opcode;

            byte[] packet = new byte[header.Length + data.Length];
            header.CopyTo(packet, 0);
            data.CopyTo(packet, header.Length);

            Serilog.Log.Information($"Sent {packet.Length} bytes with (opcode: {opcode}, header size: {header.Length}, body size: {data.Length}, body: {json}).");
            await _stream.WriteAsync(packet, 0, packet.Length, cancellationToken);
        }
        finally
        {
            _writerSemaphore.Release();
        }
    }

    public async Task ReceiveLoopAsync(CancellationToken cancellationToken)
    {
        Serilog.Log.Information("Start receiving.");
        _state = SessionState.WaitingForLength;

        byte[] buffer = new byte[1024];
        while (!cancellationToken.IsCancellationRequested)
        {
            await _readerSemaphore.WaitAsync();

            int bytesRead;
            try
            {
                //bytesRead = await _stream.ReadAsync(buffer, cancellationToken);

                // Async read on IO streams do not support timeout functionality, but sync read on IO
                // streams do support timeouts. However, the blocking read must be done on a separate
                // background thread.
                bytesRead = await Task.Run(() => 
                {
                    try
                    {
                        return _stream.Read(buffer, 0, buffer.Length);
                    }
                    catch (IOException e)
                    {
                        throw e;
                    }
                });

                if (bytesRead == 0)
                {
                    Serilog.Log.Information("Connection shutdown gracefully.");
                    Dispose();
                    break;
                }
            }
            catch (IOException)
            {
                OnTimeout?.Invoke(this, EventArgs.Empty);
                continue;
            }
            finally
            {
                _readerSemaphore.Release();
            }

            OnData?.Invoke(this, EventArgs.Empty);
            await ProcessBytesAsync(buffer, bytesRead, cancellationToken);
        }

        _state = SessionState.Idle;
    }

    private async Task ProcessBytesAsync(byte[] receivedBytes, int length, CancellationToken cancellationToken)
    {
        Serilog.Log.Information($"{length} bytes received");

        switch (_state)
        {
            case SessionState.WaitingForLength:
                await HandleLengthBytesAsync(receivedBytes, 0, length, cancellationToken);
                break;
            case SessionState.WaitingForData:
                await HandlePacketBytesAsync(receivedBytes, 0, length, cancellationToken);
                break;
            default:
                Serilog.Log.Warning($"Data received is unhandled in current session state {_state}");
                break;
        }
    }

    private async Task HandleLengthBytesAsync(byte[] receivedBytes, int offset, int length, CancellationToken cancellationToken)
    {
        int bytesToRead = Math.Min(LengthBytes, length);
        Buffer.BlockCopy(receivedBytes, offset, _buffer, _bytesRead, bytesToRead);
        _bytesRead += bytesToRead;

        Serilog.Log.Information($"handleLengthBytes: Read {bytesToRead} bytes from packet");

        if (_bytesRead >= LengthBytes)
        {
            _state = SessionState.WaitingForData;
            _packetLength = BinaryPrimitives.ReadInt32LittleEndian(_buffer);
            _bytesRead = 0;

            Serilog.Log.Information($"Packet length header received from: {_packetLength}");

            if (_packetLength > MaximumPacketLength)
            {
                Serilog.Log.Error($"Maximum packet length is 32kB, killing stream: {_packetLength}");
                Dispose();
                _state = SessionState.Disconnected;
                throw new InvalidOperationException($"Stream killed due to packet length ({_packetLength}) exceeding maximum 32kB packet size.");
            }

            if (length > bytesToRead)
            {
                await HandlePacketBytesAsync(receivedBytes, offset + bytesToRead, length - bytesToRead, cancellationToken);
            }
        }
    }

    private async Task HandlePacketBytesAsync(byte[] receivedBytes, int offset, int length, CancellationToken cancellationToken)
    {
        int bytesToRead = Math.Min(_packetLength, length);
        Buffer.BlockCopy(receivedBytes, offset, _buffer, _bytesRead, bytesToRead);
        _bytesRead += bytesToRead;

        Serilog.Log.Information($"handlePacketBytes: Read {bytesToRead} bytes from packet");

        if (_bytesRead >= _packetLength)
        {
            Serilog.Log.Information($"Packet finished receiving of {_packetLength} bytes.");
            await HandleNextPacketAsync(cancellationToken);

            _state = SessionState.WaitingForLength;
            _packetLength = 0;
            _bytesRead = 0;

            if (length > bytesToRead)
            {
                await HandleLengthBytesAsync(receivedBytes, offset + bytesToRead, length - bytesToRead, cancellationToken);
            }
        }
    }

    private async Task HandleNextPacketAsync(CancellationToken cancellationToken)
    {
        Serilog.Log.Information($"Processing packet of {_bytesRead} bytes");

        Opcode opcode = (Opcode)_buffer[0];
        int packetLength = _packetLength;
        string body = packetLength > 1 ? Encoding.UTF8.GetString(_buffer, 1, packetLength - 1) : null;

        Serilog.Log.Information($"Received body: {body}");
        await HandlePacketAsync(opcode, body, cancellationToken);
    }

    private async Task HandlePacketAsync(Opcode opcode, string body, CancellationToken cancellationToken)
    {
        Serilog.Log.Information($"Received message with opcode {opcode}.");

        switch (opcode)
        {
            case Opcode.Play:
                OnPlay?.Invoke(this, JsonSerializer.Deserialize<PlayMessage>(body));
                break;
            case Opcode.Pause:
                OnPause?.Invoke(this, EventArgs.Empty);
                break;
            case Opcode.Resume:
                OnResume?.Invoke(this, EventArgs.Empty);
                break;
            case Opcode.Stop:
                OnStop?.Invoke(this, EventArgs.Empty);
                break;
            case Opcode.Seek:
                OnSeek?.Invoke(this, JsonSerializer.Deserialize<SeekMessage>(body));
                break;
            case Opcode.SetVolume:
                OnSetVolume?.Invoke(this, JsonSerializer.Deserialize<SetVolumeMessage>(body));
                break;
            case Opcode.SetSpeed:
                OnSetSpeed?.Invoke(this, JsonSerializer.Deserialize<SetSpeedMessage>(body));
                break;
            case Opcode.PlaybackUpdate:
                HandleMessage<PlaybackUpdateMessage>(body, "Received playback update");
                break;
            case Opcode.VolumeUpdate:
                HandleMessage<VolumeUpdateMessage>(body, "Received volume update");
                break;
            case Opcode.PlaybackError:
                HandleMessage<PlaybackErrorMessage>(body, "Received playback error");
                break;
            case Opcode.Version:
                OnVersion?.Invoke(this, JsonSerializer.Deserialize<VersionMessage>(body));
                break;
            case Opcode.Ping:
                await SendMessageAsync(Opcode.Pong, cancellationToken);
                OnPing?.Invoke(this, EventArgs.Empty);
                break;
            case Opcode.Pong:
                OnPong?.Invoke(this, EventArgs.Empty);
                break;
            default:
                Serilog.Log.Warning($"Error handling packet with opcode '{opcode}' and body '{body}'");
                break;
        }
    }

    private void HandleMessage<T>(string body, string logMessage) where T : class
    {
        if (!string.IsNullOrEmpty(body))
        {
            T message = JsonSerializer.Deserialize<T>(body);
            if (message != null)
            {
                Serilog.Log.Information($"{logMessage} {JsonSerializer.Serialize(message)}");
            }
            else
            {
                Serilog.Log.Information($"{logMessage} with malformed body.");
            }
        }
        else
        {
            Serilog.Log.Information($"{logMessage} with no body.");
        }
    }

    public void Dispose()
    {
        OnDispose?.Invoke(this, EventArgs.Empty);
        _stream.Dispose();
    }
}

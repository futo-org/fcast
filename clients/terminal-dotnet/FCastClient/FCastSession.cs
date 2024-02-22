namespace FCastClient;

using System;
using System.Buffers.Binary;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;

public enum SessionState
{
    Idle,
    WaitingForLength,
    WaitingForData,
    Disconnected
}

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

            Console.WriteLine($"Sent {header.Length} bytes with (opcode: {opcode}, header size: {header.Length}, no body).");
            await _stream.WriteAsync(header, cancellationToken);
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

            Console.WriteLine($"Sent {packet.Length} bytes with (opcode: {opcode}, header size: {header.Length}, body size: {data.Length}, body: {json}).");
            await _stream.WriteAsync(packet, cancellationToken);
        }
        finally
        {
            _writerSemaphore.Release();
        }
    }

    public async Task ReceiveLoopAsync(CancellationToken cancellationToken)
    {
        Console.WriteLine("Start receiving.");
        _state = SessionState.WaitingForLength;

        byte[] buffer = new byte[1024];
        while (!cancellationToken.IsCancellationRequested)
        {
            await _readerSemaphore.WaitAsync();

            int bytesRead;
            try
            {
                bytesRead = await _stream.ReadAsync(buffer, cancellationToken);
                if (bytesRead == 0)
                {
                    Console.WriteLine("Connection shutdown gracefully.");
                    Dispose();
                    break;
                }
            }
            finally
            {
                _readerSemaphore.Release();
            }

            await ProcessBytesAsync(buffer, bytesRead, cancellationToken);
        }

        _state = SessionState.Idle;
    }

    private async Task ProcessBytesAsync(byte[] receivedBytes, int length, CancellationToken cancellationToken)
    {
        Console.WriteLine($"{length} bytes received");

        switch (_state)
        {
            case SessionState.WaitingForLength:
                await HandleLengthBytesAsync(receivedBytes, 0, length, cancellationToken);
                break;
            case SessionState.WaitingForData:
                await HandlePacketBytesAsync(receivedBytes, 0, length, cancellationToken);
                break;
            default:
                Console.WriteLine($"Data received is unhandled in current session state {_state}");
                break;
        }
    }

    private async Task HandleLengthBytesAsync(byte[] receivedBytes, int offset, int length, CancellationToken cancellationToken)
    {
        int bytesToRead = Math.Min(LengthBytes, length);
        Buffer.BlockCopy(receivedBytes, offset, _buffer, 0, bytesToRead);
        _bytesRead += bytesToRead;

        Console.WriteLine($"handleLengthBytes: Read {bytesToRead} bytes from packet");

        if (_bytesRead >= LengthBytes)
        {
            _state = SessionState.WaitingForData;
            _packetLength = BinaryPrimitives.ReadInt32LittleEndian(_buffer);
            _bytesRead = 0;

            Console.WriteLine($"Packet length header received from: {_packetLength}");

            if (_packetLength > MaximumPacketLength)
            {
                Console.WriteLine($"Maximum packet length is 32kB, killing stream: {_packetLength}");
                Dispose();
                _state = SessionState.Disconnected;
                throw new InvalidOperationException($"Stream killed due to packet length ({_packetLength}) exceeding maximum 32kB packet size.");
            }

            if (length > bytesToRead)
            {
                await HandlePacketBytesAsync(receivedBytes, bytesToRead, length - bytesToRead, cancellationToken);
            }
        }
    }

    private async Task HandlePacketBytesAsync(byte[] receivedBytes, int offset, int length, CancellationToken cancellationToken)
    {
        int bytesToRead = Math.Min(_packetLength, length);
        Buffer.BlockCopy(receivedBytes, offset, _buffer, 0, bytesToRead);
        _bytesRead += bytesToRead;

        Console.WriteLine($"handlePacketBytes: Read {bytesToRead} bytes from packet");

        if (_bytesRead >= _packetLength)
        {
            Console.WriteLine($"Packet finished receiving of {_packetLength} bytes.");
            await HandleNextPacketAsync(cancellationToken);

            _state = SessionState.WaitingForLength;
            _packetLength = 0;
            _bytesRead = 0;

            if (length > bytesToRead)
            {
                await HandleLengthBytesAsync(receivedBytes, bytesToRead, length - bytesToRead, cancellationToken);
            }
        }
    }

    private async Task HandleNextPacketAsync(CancellationToken cancellationToken)
    {
        Console.WriteLine($"Processing packet of {_bytesRead} bytes");

        Opcode opcode = (Opcode)_buffer[0];
        int packetLength = _packetLength;
        string? body = packetLength > 1 ? Encoding.UTF8.GetString(_buffer, 1, packetLength - 1) : null;

        Console.WriteLine($"Received body: {body}");
        await HandlePacketAsync(opcode, body, cancellationToken);
    }

    private async Task HandlePacketAsync(Opcode opcode, string? body, CancellationToken cancellationToken)
    {
        Console.WriteLine($"Received message with opcode {opcode}.");

        switch (opcode)
        {
            case Opcode.PlaybackUpdate:
                HandleMessage<PlaybackUpdateMessage>(body!, "Received playback update");
                break;
            case Opcode.VolumeUpdate:
                HandleMessage<VolumeUpdateMessage>(body!, "Received volume update");
                break;
            case Opcode.PlaybackError:
                HandleMessage<PlaybackErrorMessage>(body!, "Received playback error");
                break;
            case Opcode.Version:
                HandleMessage<VersionMessage>(body!, "Received version");
                break;
            case Opcode.Ping:
                Console.WriteLine("Received ping");
                await SendMessageAsync(Opcode.Pong, cancellationToken);
                Console.WriteLine("Sent pong");
                break;
            default:
                Console.WriteLine("Error handling packet");
                break;
        }
    }

    private void HandleMessage<T>(string body, string logMessage) where T : class
    {
        if (!string.IsNullOrEmpty(body))
        {
            T? message = JsonSerializer.Deserialize<T>(body);
            if (message != null)
            {
                Console.WriteLine($"{logMessage} {JsonSerializer.Serialize(message)}");
            }
            else
            {
                Console.WriteLine($"{logMessage} with malformed body.");
            }
        }
        else
        {
            Console.WriteLine($"{logMessage} with no body.");
        }
    }

    public void Dispose()
    {
        _stream.Dispose();
    }
}
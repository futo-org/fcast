using System.IO;
using System;
using System.Net.WebSockets;
using System.Threading;

public class WebSocketStream : Stream
{
    private readonly WebSocket _webSocket;

    public WebSocketStream(WebSocket webSocket)
    {
        _webSocket = webSocket;
    }

    public override bool CanRead => true;
    public override bool CanSeek => false;
    public override bool CanWrite => true;
    public override long Length => throw new NotSupportedException();
    public override long Position
    {
        get => throw new NotSupportedException();
        set => throw new NotSupportedException();
    }

    public override void Flush() { }

    public override int Read(byte[] buffer, int offset, int count)
    {
        var segment = new ArraySegment<byte>(buffer, offset, count);
        var result = _webSocket.ReceiveAsync(segment, CancellationToken.None).Result;
        return result.Count;
    }

    public override void Write(byte[] buffer, int offset, int count)
    {
        var segment = new ArraySegment<byte>(buffer, offset, count);
        _webSocket.SendAsync(segment, WebSocketMessageType.Binary, true, CancellationToken.None).Wait();
    }

    public override long Seek(long offset, SeekOrigin origin)
    {
        throw new NotSupportedException();
    }

    public override void SetLength(long value)
    {
        throw new NotSupportedException();
    }
}

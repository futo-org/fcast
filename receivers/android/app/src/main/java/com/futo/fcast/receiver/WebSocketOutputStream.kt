import org.java_websocket.WebSocket
import java.io.IOException
import java.io.OutputStream
import java.nio.ByteBuffer

class WebSocketOutputStream(private val _webSocket: WebSocket) : OutputStream() {
    @Throws(IOException::class)
    override fun write(b: Int) {
        write(byteArrayOf(b.toByte()), 0, 1)
    }

    @Throws(IOException::class)
    override fun write(b: ByteArray, off: Int, len: Int) {
        _webSocket.send(ByteBuffer.wrap(b, off, len))
    }

    @Throws(IOException::class)
    override fun close() {
        _webSocket.close()
    }
}
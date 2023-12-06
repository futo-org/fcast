import android.util.Log
import com.futo.fcast.receiver.FCastSession
import com.futo.fcast.receiver.NetworkService
import org.java_websocket.WebSocket
import org.java_websocket.handshake.ClientHandshake
import org.java_websocket.server.WebSocketServer
import java.net.InetSocketAddress
import java.nio.ByteBuffer
import java.util.IdentityHashMap

class WebSocketListenerService(private val _networkService: NetworkService, private val _onNewSession: (session: FCastSession) -> Unit) : WebSocketServer(InetSocketAddress(PORT)) {
    private var _sessions = IdentityHashMap<WebSocket, FCastSession>()

    override fun onOpen(conn: WebSocket, handshake: ClientHandshake) {
        val session = FCastSession(WebSocketOutputStream(conn), conn.remoteSocketAddress, _networkService)
        synchronized(_sessions) {
            _sessions[conn] = session
        }
        _onNewSession(session)

        Log.i(TAG, "New connection from ${conn.remoteSocketAddress}")
    }

    override fun onClose(conn: WebSocket, code: Int, reason: String, remote: Boolean) {
        synchronized(_sessions) {
            _sessions.remove(conn)
        }

        Log.i(TAG, "Closed connection from ${conn.remoteSocketAddress}")
    }

    override fun onMessage(conn: WebSocket?, message: String?) {
        Log.i(TAG, "Received string message, but not processing: $message")
    }

    override fun onMessage(conn: WebSocket?, message: ByteBuffer?) {
        if (message == null) {
            Log.i(TAG, "Received byte message null")
            return
        }

        Log.i(TAG, "Received byte message (offset = ${message.arrayOffset()}, size = ${message.remaining()})")

        synchronized(_sessions) {
            _sessions[conn]?.processBytes(message)
        }
    }

    override fun onError(conn: WebSocket?, ex: Exception) {
        Log.e(TAG, "Error in WebSocket connection", ex)
    }

    override fun onStart() {
        Log.i(TAG, "WebSocketListenerService started on port $PORT")
    }

    fun forEachSession(handler: (FCastSession) -> Unit) {
        synchronized(_sessions) {
            for (pair in _sessions) {
                handler(pair.value)
            }
        }
    }

    companion object {
        const val TAG = "WebSocketListenerService"
        const val PORT = 46898
    }
}
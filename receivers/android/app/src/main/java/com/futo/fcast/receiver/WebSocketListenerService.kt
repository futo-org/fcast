package com.futo.fcast.receiver

import android.util.Log
import org.java_websocket.WebSocket
import org.java_websocket.handshake.ClientHandshake
import org.java_websocket.server.WebSocketServer
import java.net.InetSocketAddress
import java.nio.ByteBuffer

class WebSocketListenerService(private val _networkService: NetworkService, private val _onNewSession: (session: FCastSession) -> Unit) : WebSocketServer(InetSocketAddress(PORT)) {
    private val _sockets = arrayListOf<WebSocket>()

    override fun onOpen(conn: WebSocket, handshake: ClientHandshake) {
        val session = FCastSession(WebSocketOutputStream(conn), conn.remoteSocketAddress, _networkService)
        conn.setAttachment(session)

        synchronized(_sockets) {
            _sockets.add(conn)
        }

        _onNewSession(session)

        Log.i(TAG, "New connection from ${conn.remoteSocketAddress} ${session.id}")
    }

    override fun onClose(conn: WebSocket, code: Int, reason: String, remote: Boolean) {
        synchronized(_sockets) {
            _sockets.remove(conn)
        }

        Log.i(TAG, "Closed connection from ${conn.remoteSocketAddress} ${conn.getAttachment<FCastSession>().id}")
    }

    override fun onMessage(conn: WebSocket?, message: String?) {
        if (conn == null) {
            Log.i(TAG, "Conn is null, ignore onMessage")
            return
        }

        Log.i(TAG, "Received string message, but not processing: $message")
    }

    override fun onMessage(conn: WebSocket?, message: ByteBuffer?) {
        if (conn == null) {
            Log.i(TAG, "Conn is null, ignore onMessage")
            return
        }

        if (message == null) {
            Log.i(TAG, "Received byte message null")
            return
        }

        val session = conn.getAttachment<FCastSession>()
        Log.i(TAG, "Received byte message (offset = ${message.arrayOffset()}, size = ${message.remaining()}, id = ${session.id})")
        session.processBytes(message)
    }

    override fun onError(conn: WebSocket?, ex: Exception) {
        Log.e(TAG, "Error in WebSocket connection", ex)
    }

    override fun onStart() {
        Log.i(TAG, "WebSocketListenerService started on port $PORT")
    }

    fun forEachSession(handler: (FCastSession) -> Unit) {
        synchronized(_sockets) {
            _sockets.forEach {
                handler(it.getAttachment())
            }
        }
    }

    companion object {
        const val TAG = "WebSocketListenerService"
        const val PORT = 46898
    }
}
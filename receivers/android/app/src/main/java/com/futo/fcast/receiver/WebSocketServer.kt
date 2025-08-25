package com.futo.fcast.receiver

import android.util.Log
import org.java_websocket.WebSocket
import org.java_websocket.handshake.ClientHandshake
import org.java_websocket.server.WebSocketServer
import java.net.InetSocketAddress
import java.nio.ByteBuffer
import java.util.UUID

class WebSocketServer(
    private val _networkService: NetworkService,
    private val _onNewSession: (session: FCastSession) -> Unit,
    private val _onOpen: (session: FCastSession, socket: WebSocket) -> Unit,
    private val _onClose: (session: FCastSession) -> Unit,
    private val _disconnect: (sessionId: UUID) -> Unit,
    private val port: Int) : WebSocketServer(InetSocketAddress(port)) {

    private val _sockets = arrayListOf<WebSocket>()

    override fun onOpen(conn: WebSocket, handshake: ClientHandshake) {
        val session = FCastSession(WebSocketOutputStream(conn), conn.remoteSocketAddress, _networkService)
        conn.setAttachment(session)

        _onOpen(session, conn)
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

        val session = conn.getAttachment<FCastSession>()
        _onClose(session)

        Log.i(TAG, "Closed connection from ${conn.remoteSocketAddress} ${session.id}")
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

        if (!message.hasArray()) {
            throw Exception("message ByteBuffer does not have a backing array")
        }
        val byteArray = message.array()
        val offset = message.arrayOffset() + message.position()
        val length = message.remaining()

        session.processBytes(byteArray, length, offset)
    }

    override fun onError(conn: WebSocket?, ex: Exception) {
        Log.e(TAG, "Error in WebSocket connection", ex)
        conn?.getAttachment<FCastSession>()?.let { _disconnect(it.id) }
    }

    override fun onStart() {}

    companion object {
        private const val TAG = "WebSocketServer"
    }
}

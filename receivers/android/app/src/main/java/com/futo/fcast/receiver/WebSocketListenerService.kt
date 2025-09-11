package com.futo.fcast.receiver

import android.util.Log
import org.java_websocket.WebSocket
import java.util.UUID

class WebSocketListenerService(
    private val _networkService: NetworkService,
    private val _onNewSession: (session: FCastSession) -> Unit
) : ListenerService() {
    private var _stopped: Boolean = true
    private val _sockets = arrayListOf<WebSocket>()
    private val _server =
        WebSocketServer(_networkService, _onNewSession, ::onOpen, ::onClose, ::disconnect, PORT)
    private val _socketMap: MutableMap<UUID, WebSocket> = mutableMapOf()

    override fun start() {
        if (!_stopped) {
            return
        }
        _stopped = false

        _server.start()
        Log.i(TAG, "WebSocketListenerService started on port $PORT")
    }

    override fun stop() {
        if (_stopped) {
            return
        }
        _stopped = true

        _server.stop()
        Log.i(TAG, "Stopped WebSocketListenerService")
    }

    override fun disconnect(sessionId: UUID) {
        sessionMap[sessionId]?.close()
        _socketMap[sessionId]?.close()
        Log.i(TAG, "Disconnected ${_socketMap[sessionId]?.remoteSocketAddress}")
    }

    fun forEachSession(handler: (FCastSession) -> Unit) {
        synchronized(_sockets) {
            _sockets.forEach {
                handler(it.getAttachment())
            }
        }
    }

    private fun onOpen(session: FCastSession, socket: WebSocket) {
        synchronized(sessionMap) {
            sessionMap[session.id] = session
        }
        synchronized(_socketMap) {
            _socketMap[session.id] = socket
        }

        _networkService.onConnect(this, session.id, socket.remoteSocketAddress)
    }

    private fun onClose(session: FCastSession) {
        _socketMap[session.id]?.let {
            _networkService.onDisconnect(session.id, it.remoteSocketAddress)
        }

        synchronized(sessionMap) {
            sessionMap.remove(session.id)
        }

        synchronized(_socketMap) {
            _socketMap.remove(session.id)
        }
    }

    companion object {
        private const val TAG = "WebSocketListenerService"
        const val PORT = 46898
    }
}

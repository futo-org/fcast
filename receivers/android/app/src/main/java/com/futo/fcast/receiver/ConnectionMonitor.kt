package com.futo.fcast.receiver

import android.util.Log
import com.futo.fcast.receiver.models.Opcode
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import java.net.SocketAddress
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap

class ConnectionMonitor(private val _scope: CoroutineScope) {
    init {
        setInterval({
            if (_backendConnections.isNotEmpty()) {
                val keys = _backendConnections.keys.toSet()
                var removeSession = false

                for (sessionId in keys) {
                    _backendConnections[sessionId]?.let {
                        val version = it.getSessionProtocolVersion(sessionId)

                        if (version != null && version >= 2) {
                            if (_heartbeatRetries.getOrDefault(sessionId, 0) > 3) {
                                Log.w(
                                    TAG,
                                    "Could not ping device with connection id $sessionId. Disconnecting..."
                                )
                                it.disconnect(sessionId)
                                continue
                            }

                            _scope.launch(Dispatchers.IO) {
                                try {
                                    Log.d(
                                        TAG,
                                        "Pinging session $sessionId with ${_heartbeatRetries[sessionId]} retries left"
                                    )
                                    it.send(Opcode.Ping, null, sessionId)
                                    _heartbeatRetries[sessionId] =
                                        _heartbeatRetries.getOrDefault(sessionId, 0) + 1
                                } catch (e: Throwable) {
                                    Log.w(TAG, "Failed to ping session $sessionId", e)
                                }
                            }
                        } else if (version == null) {
                            Log.w(
                                TAG,
                                "Session $sessionId was not found in the list of active sessions. Removing..."
                            )
                            removeSession = true
                            _heartbeatRetries.remove(sessionId)
                        }
                    }

                    if (removeSession) {
                        _backendConnections.remove(sessionId)
                    }
                }
            }
        }, CONNECTION_PING_TIMEOUT)
    }

    companion object {
        private const val TAG = "ConnectionMonitor"

        private const val CONNECTION_PING_TIMEOUT = 2500L
        private val _heartbeatRetries = ConcurrentHashMap<UUID, Int>()
        private val _backendConnections = ConcurrentHashMap<UUID, ListenerService>()
        private const val UI_CONNECT_UPDATE_TIMEOUT = 100L
        private const val UI_DISCONNECT_UPDATE_TIMEOUT =
            2000L // Senders may reconnect, but generally need more time
        private val _uiUpdateMap =
            mutableMapOf<SocketAddress, ArrayList<Pair<String, (() -> Unit)>>>()

        fun onPingPong(sessionId: UUID) {
            Log.d(TAG, "Received response from $sessionId")
            _heartbeatRetries[sessionId] = 0
        }

        fun onConnect(
            listener: ListenerService,
            sessionId: UUID,
            address: SocketAddress,
            uiUpdateCallback: () -> Unit
        ) {
            Log.i(TAG, "Device connected: sessionId=$sessionId, address=$address")

            _backendConnections[sessionId] = listener
            _heartbeatRetries[sessionId] = 0

            // Occasionally senders seem to instantaneously disconnect and reconnect, so suppress those ui updates
            val senderUpdateQueue = _uiUpdateMap.getOrDefault(address, arrayListOf())
            senderUpdateQueue.add(Pair("connect", uiUpdateCallback))
            _uiUpdateMap[address] = senderUpdateQueue

            if (senderUpdateQueue.size == 1) {
                setTimeout({ processUiUpdateCallbacks(address) }, UI_CONNECT_UPDATE_TIMEOUT)
            }
        }

        fun onDisconnect(sessionId: UUID, address: SocketAddress, uiUpdateCallback: () -> Unit) {
            Log.i(TAG, "Device disconnected: sessionId=$sessionId, address=$address")

            _backendConnections.remove(sessionId)
            _heartbeatRetries.remove(sessionId)

            val senderUpdateQueue = _uiUpdateMap.getOrDefault(address, arrayListOf())
            senderUpdateQueue.add(Pair("disconnect", uiUpdateCallback))
            _uiUpdateMap[address] = senderUpdateQueue

            if (senderUpdateQueue.size == 1) {
                setTimeout({ processUiUpdateCallbacks(address) }, UI_DISCONNECT_UPDATE_TIMEOUT)
            }
        }

        private fun processUiUpdateCallbacks(address: SocketAddress) {
            val updateQueue = _uiUpdateMap.getOrDefault(address, arrayListOf())
            var lastConnectCb: () -> Unit = {}
            var lastDisconnectCb: () -> Unit = {}
            var messageCount = 0

            for (update in updateQueue) {
                Log.d(TAG, "Processing update event '${update.first}' for $address")
                when (update.first) {
                    "connect" -> {
                        messageCount += 1
                        lastConnectCb = update.second
                    }

                    "disconnect" -> {
                        messageCount -= 1
                        lastDisconnectCb = update.second
                    }

                    else -> {
                        Log.w(TAG, "Unrecognized UI update event: ${update.first}")
                    }
                }
            }

            if (messageCount > 0) {
                Log.d(TAG, "Sending connect event for $address")
                lastConnectCb()
            } else if (messageCount < 0) {
                Log.d(TAG, "Sending disconnect event for $address")
                lastDisconnectCb()
            }

            _uiUpdateMap[address] = arrayListOf()
        }
    }
}

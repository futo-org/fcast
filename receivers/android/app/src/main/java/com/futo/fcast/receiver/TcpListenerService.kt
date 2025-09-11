package com.futo.fcast.receiver

import android.os.Looper
import android.util.Log
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.UUID

class TcpListenerService(
    private val _networkService: NetworkService,
    private val _onNewSession: (session: FCastSession) -> Unit
) : ListenerService() {
    private var _stopped: Boolean = true
    private var _listenThread: Thread? = null
    private var _serverSocket: ServerSocket? = null
    private val _clientThreads: ArrayList<Thread> = arrayListOf()
    private val _socketMap: MutableMap<UUID, Socket> = mutableMapOf()

    override fun start() {
        Log.i(TAG, "Starting TcpListenerService")
        if (!_stopped) {
            return
        }
        _stopped = false

        _listenThread = Thread {
            Log.i(TAG, "Starting listener")

            try {
                listenForIncomingConnections()
            } catch (e: Throwable) {
                Log.e(TAG, "Stopped listening for connections due to an unexpected error", e)
            }
        }

        _listenThread?.start()

        Log.i(TAG, "Started TcpListenerService")
    }

    override fun stop() {
        Log.i(TAG, "Stopping TcpListenerService")
        if (_stopped) {
            return
        }
        _stopped = true

        _serverSocket?.close()
        _serverSocket = null

        _listenThread?.join()
        _listenThread = null

        synchronized(_clientThreads) {
            _clientThreads.clear()
        }

        Log.i(TAG, "Stopped TcpListenerService")
    }

    override fun disconnect(sessionId: UUID) {
        try {
            sessionMap[sessionId]?.close()
            _socketMap[sessionId]?.let {
                it.close()
                _networkService.onDisconnect(sessionId, it.remoteSocketAddress)
                Log.i(TAG, "Disconnected id=$sessionId, address=${it.remoteSocketAddress}")
            }

            synchronized(sessionMap) {
                sessionMap.remove(sessionId)
            }

            synchronized(_socketMap) {
                _socketMap.remove(sessionId)
            }

            synchronized(_clientThreads) {
                _clientThreads.remove(Thread.currentThread())
            }
        } catch (e: Throwable) {
            Log.e(TAG, "Failed to close client socket", e)
        }
    }

    fun getSenders(): ArrayList<String> {
        val senders = arrayListOf<String>()
        _socketMap.toList().mapTo(senders) { it.second.remoteSocketAddress.toString() }
        return senders;
    }

    fun forEachSession(handler: (FCastSession) -> Unit) {
        synchronized(sessionMap) {
            for (session in sessionMap) {
                handler(session.value)
            }
        }
    }

    private fun listenForIncomingConnections() {
        Log.i(TAG, "Started listening for incoming connections")

        while (!_stopped) {
            try {
                _serverSocket = ServerSocket()

                try {
                    _serverSocket!!.bind(InetSocketAddress(PORT))

                    while (!_stopped) {
                        val clientSocket = _serverSocket!!.accept() ?: break

                        val clientThread = Thread {
                            Looper.prepare()
                            handleClientConnection(clientSocket)
                        }

                        synchronized(_clientThreads) {
                            _clientThreads.add(clientThread)
                        }

                        Log.i(
                            TAG,
                            "New connection received from ${clientSocket.remoteSocketAddress}"
                        )
                        clientThread.start()
                    }
                } catch (e: Throwable) {
                    Log.e(
                        TAG,
                        "Failed to accept client connection due to an error, sleeping 1 second then restarting",
                        e
                    )
                    Thread.sleep(1000)
                } finally {
                    _serverSocket?.close()
                    _serverSocket = null
                }
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to create server socket, sleeping 1 second then restarting", e)
                Thread.sleep(1000)
            }
        }

        Log.i(TAG, "Stopped listening for incoming connections")
    }

    private fun handleClientConnection(socket: Socket) {
        val session =
            FCastSession(socket.getOutputStream(), socket.remoteSocketAddress, _networkService)

        try {
            synchronized(sessionMap) {
                sessionMap[session.id] = session
            }
            synchronized(_socketMap) {
                _socketMap[session.id] = socket
            }

            _networkService.onConnect(this, session.id, socket.remoteSocketAddress)
            _onNewSession(session)

            Log.i(TAG, "Waiting for data from ${socket.remoteSocketAddress}")

            val bufferSize = 4096
            val buffer = ByteArray(bufferSize)
            val inputStream = socket.getInputStream()

            var bytesRead: Int
            while (!_stopped) {
                bytesRead = inputStream.read(buffer, 0, bufferSize)
                if (bytesRead == -1) {
                    break
                }

                session.processBytes(buffer, bytesRead)
            }
        } catch (e: Throwable) {
            Log.e(TAG, "Failed handle client connection due to an error", e)
        } finally {
            disconnect(session.id)
        }
    }

    companion object {
        private const val TAG = "TcpListenerService"
        const val PORT = 46899
    }
}
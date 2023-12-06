package com.futo.fcast.receiver

import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.BufferedInputStream
import java.net.ServerSocket
import java.net.Socket
import java.util.ArrayList

class TcpListenerService(private val _networkService: NetworkService, private val _onNewSession: (session: FCastSession) -> Unit) {
    private var _serverSocket: ServerSocket? = null
    private var _stopped: Boolean = false
    private var _listenThread: Thread? = null
    private var _clientThreads: ArrayList<Thread> = arrayListOf()
    private var _sessions: ArrayList<FCastSession> = arrayListOf()

    fun start() {
        Log.i(TAG, "Starting TcpListenerService")

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

    fun stop() {
        Log.i(TAG, "Stopping TcpListenerService")

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

    fun forEachSession(handler: (FCastSession) -> Unit) {
        synchronized(_sessions) {
            for (session in _sessions) {
                handler(session)
            }
        }
    }

    private fun listenForIncomingConnections() {
        Log.i(TAG, "Started listening for incoming connections")

        _serverSocket = ServerSocket(PORT)

        while (!_stopped) {
            val clientSocket = _serverSocket?.accept() ?: break

            val clientThread = Thread {
                try {
                    handleClientConnection(clientSocket)
                } catch (e: Throwable) {
                    Log.e(TAG, "Failed handle client connection due to an error", e)
                }
            }

            synchronized(_clientThreads) {
                _clientThreads.add(clientThread)
            }

            clientThread.start()
        }

        Log.i(TAG, "Stopped listening for incoming connections")
    }

    private fun handleClientConnection(socket: Socket) {
        Log.i(TAG, "New connection received from ${socket.remoteSocketAddress}")

        val session = FCastSession(socket.getOutputStream(), socket.remoteSocketAddress, _networkService)
        synchronized(_sessions) {
            _sessions.add(session)
        }
        _onNewSession(session)

        Log.i(TAG, "Waiting for data from ${socket.remoteSocketAddress}")

        val bufferSize = 4096
        val buffer = ByteArray(bufferSize)
        val inputStream = BufferedInputStream(socket.getInputStream())

        var bytesRead: Int
        while (!_stopped) {
            bytesRead = inputStream.read(buffer, 0, bufferSize)
            if (bytesRead == -1) {
                break
            }

            session.processBytes(buffer, bytesRead)
        }

        socket.close()

        synchronized(_sessions) {
            _sessions.remove(session)
        }

        synchronized(_clientThreads) {
            _clientThreads.remove(Thread.currentThread())
        }

        Log.i(TAG, "Disconnected ${socket.remoteSocketAddress}")
    }

    companion object {
        const val TAG = "TcpListenerService"
        const val PORT = 46899
    }
}
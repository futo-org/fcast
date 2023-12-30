package com.futo.fcast.receiver

import SslKeyManager
import android.util.Log
import java.io.BufferedInputStream
import java.net.Socket
import java.security.KeyStore
import java.security.cert.Certificate
import javax.net.ssl.KeyManagerFactory
import javax.net.ssl.SSLContext
import javax.net.ssl.SSLServerSocket
import javax.net.ssl.TrustManagerFactory

class TlsListenerService(private val _networkService: NetworkService, private val _onNewSession: (session: FCastSession) -> Unit) {
    private var _serverSocket: SSLServerSocket? = null
    private var _stopped: Boolean = false
    private var _listenThread: Thread? = null
    private var _clientThreads: ArrayList<Thread> = arrayListOf()
    private var _sessions: ArrayList<FCastSession> = arrayListOf()

    fun start(sslKeyManager: SslKeyManager) {
        Log.i(TAG, "Starting TlsListenerService")

        val serverSocketFactory = sslKeyManager.getSslServerSocketFactory()
        _serverSocket = (serverSocketFactory.createServerSocket(PORT) as SSLServerSocket)

        _listenThread = Thread {
            Log.i(TAG, "Starting TLS listener")

            try {
                listenForIncomingConnections()
            } catch (e: Throwable) {
                Log.e(TAG, "Stopped TLS listening for connections due to an unexpected error", e)
            }
        }

        _listenThread?.start()

        Log.i(TAG, "Started TlsListenerService")
    }

    fun stop() {
        Log.i(TAG, "Stopping TlsListenerService")

        _stopped = true

        _serverSocket?.close()
        _serverSocket = null

        _listenThread?.join()
        _listenThread = null

        synchronized(_clientThreads) {
            _clientThreads.clear()
        }

        Log.i(TAG, "Stopped TlsListenerService")
    }

    fun forEachSession(handler: (FCastSession) -> Unit) {
        synchronized(_sessions) {
            for (session in _sessions) {
                handler(session)
            }
        }
    }

    private fun listenForIncomingConnections() {
        Log.i(TAG, "Started TLS listening for incoming connections")

        while (!_stopped) {
            val clientSocket = _serverSocket?.accept() ?: break

            val clientThread = Thread {
                try {
                    handleClientConnection(clientSocket)
                } catch (e: Throwable) {
                    Log.e(TAG, "Failed handle TLS client connection due to an error", e)
                }
            }

            synchronized(_clientThreads) {
                _clientThreads.add(clientThread)
            }

            clientThread.start()
        }

        Log.i(TAG, "Stopped TLS listening for incoming connections")
    }

    private fun handleClientConnection(socket: Socket) {
        Log.i(TAG, "New TLS connection received from ${socket.remoteSocketAddress}")

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
        const val TAG = "TlsListenerService"
        const val PORT = 46897
    }
}
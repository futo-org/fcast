package com.futo.fcast.receiver

import android.util.Log
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.ArrayList

class TcpListenerService(private val _networkService: NetworkService, private val _onNewSession: (session: FCastSession) -> Unit) {
    private var _stopped: Boolean = false
    private var _listenThread: Thread? = null
    private var _clientThreads: ArrayList<Thread> = arrayListOf()
    private var _sessions: ArrayList<FCastSession> = arrayListOf()
    private var _serverSocket: ServerSocket? = null

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

        while (!_stopped) {
            try {
                _serverSocket = ServerSocket()

                try {
                    _serverSocket!!.bind(InetSocketAddress(PORT))

                    while (!_stopped) {
                        val clientSocket = _serverSocket!!.accept() ?: break
                        val clientThread = Thread {
                            try {
                                Log.i(TAG, "New connection received from ${clientSocket.remoteSocketAddress}")
                                handleClientConnection(clientSocket)
                            } catch (e: Throwable) {
                                Log.e(TAG, "Failed handle client connection due to an error", e)
                            } finally {
                                try {
                                    clientSocket.close()

                                    synchronized(_clientThreads) {
                                        _clientThreads.remove(Thread.currentThread())
                                    }

                                    Log.i(TAG, "Disconnected ${clientSocket.remoteSocketAddress}")
                                } catch (e: Throwable) {
                                    Log.e(TAG, "Failed to close client socket", e)
                                }
                            }
                        }

                        synchronized(_clientThreads) {
                            _clientThreads.add(clientThread)
                        }

                        clientThread.start()
                    }
                } catch (e: Throwable) {
                    Log.e(TAG, "Failed to accept client connection due to an error, sleeping 1 second then restarting", e)
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
        val session = FCastSession(socket.getOutputStream(), socket.remoteSocketAddress, _networkService)

        try {
            synchronized(_sessions) {
                _sessions.add(session)
            }
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
        } finally {
            synchronized(_sessions) {
                _sessions.remove(session)
            }
        }
    }

    companion object {
        const val TAG = "TcpListenerService"
        const val PORT = 46899
    }
}
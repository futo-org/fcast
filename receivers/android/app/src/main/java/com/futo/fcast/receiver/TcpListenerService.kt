package com.futo.fcast.receiver

import android.app.*
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.IBinder
import android.provider.Settings
import android.util.Log
import android.widget.Toast
import androidx.core.app.NotificationCompat
import kotlinx.coroutines.*
import java.io.BufferedInputStream
import java.net.NetworkInterface
import java.net.ServerSocket
import java.net.Socket
import java.util.*

class TcpListenerService : Service() {
    private var _serverSocket: ServerSocket? = null
    private var _stopped: Boolean = false
    private var _listenThread: Thread? = null
    private var _clientThreads: ArrayList<Thread> = arrayListOf()
    private var _sessions: ArrayList<FCastSession> = arrayListOf()
    private var _discoveryService: DiscoveryService? = null
    private var _scope: CoroutineScope? = null

    override fun onBind(intent: Intent?): IBinder? {
        return null
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (instance != null) {
            throw Exception("Do not start service when already running")
        }

        instance = this

        Log.i(TAG, "Starting ListenerService")

        _scope = CoroutineScope(Dispatchers.Main)

        createNotificationChannel()
        val notification: Notification = NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle("TCP Listener Service")
            .setContentText("Listening on port $PORT")
            .setSmallIcon(R.mipmap.ic_launcher)
            .build()

        startForeground(NOTIFICATION_ID, notification)

        _discoveryService = DiscoveryService(this)
        _discoveryService?.start()

        _listenThread = Thread {
            Log.i(TAG, "Starting listener")

            try {
                listenForIncomingConnections()
            } catch (e: Throwable) {
                Log.e(TAG, "Stopped listening for connections due to an unexpected error", e)
            }
        }

        _listenThread?.start()

        _scope?.launch(Dispatchers.Main) {
            while (!_stopped) {
                try {
                    val player = PlayerActivity.instance
                    if (player != null) {
                        val updateMessage = PlaybackUpdateMessage(
                            player.currentPosition / 1000,
                            if (player.isPlaying) 1 else 2
                        )

                        withContext(Dispatchers.IO) {
                            try {
                                sendCastPlaybackUpdate(updateMessage)
                            } catch (eSend: Throwable) {
                                Log.e(TAG, "Unhandled error sending update", eSend)
                            }

                            Log.i(TAG, "Update sent")
                        }
                    }
                } catch (eTimer: Throwable) {
                    Log.e(TAG, "Unhandled error on timer thread", eTimer)
                } finally {
                    delay(1000)
                }
            }
        }

        Log.i(TAG, "Started ListenerService")
        Toast.makeText(this, "Started FCast service", Toast.LENGTH_LONG).show()

        return START_STICKY
    }

    override fun onDestroy() {
        super.onDestroy()

        Log.i(TAG, "Stopped ListenerService")
        _stopped = true

        _discoveryService?.stop()
        _discoveryService = null

        _serverSocket?.close()
        _serverSocket = null

        _listenThread?.join()
        _listenThread = null

        synchronized(_clientThreads) {
            _clientThreads.clear()
        }

        _scope?.cancel()
        _scope = null

        Toast.makeText(this, "Stopped FCast service", Toast.LENGTH_LONG).show()
        instance = null
    }

    private fun sendCastPlaybackUpdate(value: PlaybackUpdateMessage) {
        synchronized(_sessions) {
            for (session in _sessions) {
                try {
                    session.sendPlaybackUpdate(value)
                } catch (e: Throwable) {
                    Log.w(TAG, "Failed to send playback update", e)
                }
            }
        }
    }

    fun sendCastVolumeUpdate(value: VolumeUpdateMessage) {
        synchronized(_sessions) {
            for (session in _sessions) {
                try {
                    session.sendVolumeUpdate(value)
                } catch (e: Throwable) {
                    Log.w(TAG, "Failed to send volume update", e)
                }
            }
        }
    }

    fun onCastPlay(playMessage: PlayMessage) {
        Log.i(TAG, "onPlay")

        _scope?.launch {
            try {
                if (PlayerActivity.instance == null) {
                    val i = Intent(this@TcpListenerService, PlayerActivity::class.java)
                    i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    i.putExtra("container", playMessage.container)
                    i.putExtra("url", playMessage.url)
                    i.putExtra("content", playMessage.content)
                    i.putExtra("time", playMessage.time)

                    if (activityCount > 0) {
                        startActivity(i)
                    } else if (Settings.canDrawOverlays(this@TcpListenerService)) {
                        val pi = PendingIntent.getActivity(this@TcpListenerService, 0, i, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
                        pi.send()
                    } else {
                        val pi = PendingIntent.getActivity(this@TcpListenerService, 0, i, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
                        val playNotification = NotificationCompat.Builder(this@TcpListenerService, CHANNEL_ID)
                            .setContentTitle("FCast")
                            .setContentText("New content received. Tap to play.")
                            .setSmallIcon(R.drawable.ic_launcher_background)
                            .setContentIntent(pi)
                            .setPriority(NotificationCompat.PRIORITY_HIGH)
                            .setAutoCancel(true)
                            .build()

                        val notificationManager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
                        notificationManager.notify(PLAY_NOTIFICATION_ID, playNotification)
                    }
                } else {
                    PlayerActivity.instance?.play(playMessage)
                }
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to play", e)
            }
        }
    }

    fun onCastPause() {
        Log.i(TAG, "onPause")

        _scope?.launch {
            try {
                PlayerActivity.instance?.pause()
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to pause", e)
            }
        }
    }

    fun onCastResume() {
        Log.i(TAG, "onResume")

        _scope?.launch {
            try {
                PlayerActivity.instance?.resume()
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to resume", e)
            }
        }
    }

    fun onCastStop() {
        Log.i(TAG, "onStop")

        _scope?.launch {
            try {
                PlayerActivity.instance?.finish()
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to stop", e)
            }
        }
    }

    fun onCastSeek(seekMessage: SeekMessage) {
        Log.i(TAG, "onSeek")

        _scope?.launch {
            try {
                PlayerActivity.instance?.seek(seekMessage)
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to seek", e)
            }
        }
    }

    fun onSetVolume(setVolumeMessage: SetVolumeMessage) {
        Log.i(TAG, "onSetVolume")

        _scope?.launch {
            try {
                PlayerActivity.instance?.setVolume(setVolumeMessage)
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to seek", e)
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

        val session = FCastSession(socket, this)
        synchronized(_sessions) {
            _sessions.add(session)
        }

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

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val name = "TCP Listener Service"
            val descriptionText = "Listening on port $PORT"
            val importance = NotificationManager.IMPORTANCE_DEFAULT
            val channel = NotificationChannel(CHANNEL_ID, name, importance).apply {
                description = descriptionText
            }

            val notificationManager: NotificationManager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            notificationManager.createNotificationChannel(channel)
        }
    }

    companion object {
        const val PORT = 46899
        const val CHANNEL_ID = "TcpListenerServiceChannel"
        const val NOTIFICATION_ID = 1
        const val PLAY_NOTIFICATION_ID = 2
        const val TAG = "TcpListenerService"
        var activityCount = 0
        var instance: TcpListenerService? = null
    }
}

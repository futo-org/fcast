package com.futo.fcast.receiver

import SslKeyManager
import WebSocketListenerService
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
import org.bouncycastle.jce.provider.BouncyCastleProvider
import java.security.Security

class NetworkService : Service() {
    private var _discoveryService: DiscoveryService? = null
    private var _stopped = false
    private var _tcpListenerService: TcpListenerService? = null
    private var _tlsListenerService: TlsListenerService? = null
    private var _webSocketListenerService: WebSocketListenerService? = null
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
        _stopped = false

        val name = "Network Listener Service"
        val descriptionText = "Listening on port ${TcpListenerService.PORT} (TCP) and port ${WebSocketListenerService.PORT} (Websocket)"

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val importance = NotificationManager.IMPORTANCE_DEFAULT
            val channel = NotificationChannel(CHANNEL_ID, name, importance).apply {
                description = descriptionText
            }

            val notificationManager: NotificationManager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            notificationManager.createNotificationChannel(channel)
        }

        val notification: Notification = createNotificationBuilder()
            .setContentTitle(name)
            .setContentText(descriptionText)
            .setSmallIcon(R.mipmap.ic_launcher)
            .build()

        startForeground(NOTIFICATION_ID, notification)

        val onNewSession: (FCastSession) -> Unit = { session ->
            _scope?.launch(Dispatchers.Main) {
                Log.i(TAG, "On new session ${session.id}")

                withContext(Dispatchers.IO) {
                    try {
                        Log.i(TAG, "Sending version ${session.id}")
                        session.send(Opcode.Version, VersionMessage(2))
                    } catch (e: Throwable) {
                        Log.e(TAG, "Failed to send version ${session.id}")
                    }
                }

                var encounteredError = false
                while (!_stopped && !encounteredError) {
                    try {
                        val updateMessage = generateUpdateMessage()
                        withContext(Dispatchers.IO) {
                            try {
                                session.send(Opcode.PlaybackUpdate, updateMessage)
                                Log.i(TAG, "Update sent ${session.id}")
                            } catch (eSend: Throwable) {
                                Log.e(TAG, "Unhandled error sending update ${session.id}", eSend)
                                encounteredError = true
                                return@withContext
                            }
                        }
                    } catch (eTimer: Throwable) {
                        Log.e(TAG, "Unhandled error on timer thread ${session.id}", eTimer)
                    } finally {
                        delay(1000)
                    }
                }

                Log.i(TAG, "Send loop closed ${session.id}")
            }
        }

        _discoveryService = DiscoveryService(this).apply {
            start()
        }

        _tcpListenerService = TcpListenerService(this) { onNewSession(it) }.apply {
            start()
        }

        _webSocketListenerService = WebSocketListenerService(this) { onNewSession(it) }.apply {
            start()
        }

        val sslKeyManager = SslKeyManager("fcast_receiver")
        _tlsListenerService = TlsListenerService(this) { onNewSession(it) }.apply {
            start(sslKeyManager)
        }

        Log.i(TAG, "Started NetworkService")
        Toast.makeText(this, "Started FCast service", Toast.LENGTH_LONG).show()

        return START_STICKY
    }

    @Suppress("DEPRECATION")
    private fun createNotificationBuilder(): NotificationCompat.Builder {
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            NotificationCompat.Builder(this, CHANNEL_ID)
        } else {
            // For pre-Oreo, do not specify the channel ID
            NotificationCompat.Builder(this)
        }
    }

    override fun onDestroy() {
        super.onDestroy()

        Log.i(TAG, "Stopped NetworkService")

        _stopped = true

        _discoveryService?.stop()
        _discoveryService = null

        _tcpListenerService?.stop()
        _tcpListenerService = null

        _tlsListenerService?.stop()
        _tlsListenerService = null

        try {
            _webSocketListenerService?.stop()
        } catch (e: Throwable) {
            //Ignored
        } finally {
            _webSocketListenerService = null
        }

        _scope?.cancel()
        _scope = null

        Toast.makeText(this, "Stopped FCast service", Toast.LENGTH_LONG).show()
        instance = null
    }

    fun generateUpdateMessage(): PlaybackUpdateMessage {
        val player = PlayerActivity.instance
        return  if (player != null) {
            PlaybackUpdateMessage(
                System.currentTimeMillis(),
                player.currentPosition / 1000.0,
                player.duration / 1000.0,
                if (player.isPlaying) 1 else 2,
                player.speed.toDouble()
            )
        } else {
            PlaybackUpdateMessage(
                System.currentTimeMillis(),
                0.0,
                0.0,
                0,
                0.0
            )
        }
    }

    private inline fun <reified T> send(opcode: Opcode, message: T) {
        val sender: (FCastSession) -> Unit = { session: FCastSession ->
            _scope?.launch(Dispatchers.IO) {
                try {
                    session.send(opcode, message)
                    Log.i(TAG, "Playback error sent ${session.id}")
                } catch (e: Throwable) {
                    Log.w(TAG, "Failed to send playback error", e)
                }
            }
        }

        _tcpListenerService?.forEachSession(sender)
        _webSocketListenerService?.forEachSession(sender)
        _tlsListenerService?.forEachSession(sender)
    }

    fun sendPlaybackError(error: String) {
        val message = PlaybackErrorMessage(error)
        send(Opcode.PlaybackError, message)
    }

    fun sendPlaybackUpdate(message: PlaybackUpdateMessage) {
        send(Opcode.PlaybackUpdate, message)
    }

    fun sendCastVolumeUpdate(value: VolumeUpdateMessage) {
        send(Opcode.VolumeUpdate, value)
    }

    fun onCastPlay(playMessage: PlayMessage) {
        Log.i(TAG, "onPlay")

        _scope?.launch(Dispatchers.Main) {
            try {
                if (PlayerActivity.instance == null) {
                    val i = Intent(this@NetworkService, PlayerActivity::class.java)
                    i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    i.putExtra("container", playMessage.container)
                    i.putExtra("url", playMessage.url)
                    i.putExtra("content", playMessage.content)
                    i.putExtra("time", playMessage.time)
                    i.putExtra("speed", playMessage.speed)

                    if (activityCount > 0) {
                        startActivity(i)
                    } else if (Settings.canDrawOverlays(this@NetworkService)) {
                        val pi = PendingIntent.getActivity(this@NetworkService, 0, i, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
                        pi.send()
                    } else {
                        val pi = PendingIntent.getActivity(this@NetworkService, 0, i, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
                        val playNotification = createNotificationBuilder()
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

        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.pause()
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to pause", e)
            }
        }
    }

    fun onCastResume() {
        Log.i(TAG, "onResume")

        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.resume()
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to resume", e)
            }
        }
    }

    fun onCastStop() {
        Log.i(TAG, "onStop")

        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.finish()
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to stop", e)
            }
        }
    }

    fun onCastSeek(seekMessage: SeekMessage) {
        Log.i(TAG, "onSeek")

        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.seek(seekMessage)
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to seek", e)
            }
        }
    }

    fun onSetVolume(setVolumeMessage: SetVolumeMessage) {
        Log.i(TAG, "onSetVolume")

        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.setVolume(setVolumeMessage)
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to seek", e)
            }
        }
    }

    fun onSetSpeed(setSpeedMessage: SetSpeedMessage) {
        Log.i(TAG, "setSpeedMessage")

        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.setSpeed(setSpeedMessage)
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to seek", e)
            }
        }
    }

    companion object {
        private const val CHANNEL_ID = "NetworkListenerServiceChannel"
        private const val NOTIFICATION_ID = 1
        private const val PLAY_NOTIFICATION_ID = 2
        const val TAG = "NetworkService"
        var activityCount = 0
        var instance: NetworkService? = null
    }
}

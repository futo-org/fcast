package com.futo.fcast.receiver

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.provider.Settings
import android.util.Log
import android.widget.Toast
import androidx.core.app.NotificationCompat
import com.futo.fcast.receiver.composables.frontendConnections
import com.futo.fcast.receiver.models.ContentObject
import com.futo.fcast.receiver.models.ContentType
import com.futo.fcast.receiver.models.EventMessage
import com.futo.fcast.receiver.models.InitialSenderMessage
import com.futo.fcast.receiver.models.Opcode
import com.futo.fcast.receiver.models.PROTOCOL_VERSION
import com.futo.fcast.receiver.models.PlayMessage
import com.futo.fcast.receiver.models.PlayUpdateMessage
import com.futo.fcast.receiver.models.PlaybackErrorMessage
import com.futo.fcast.receiver.models.PlaybackUpdateMessage
import com.futo.fcast.receiver.models.PlaylistContent
import com.futo.fcast.receiver.models.SeekMessage
import com.futo.fcast.receiver.models.SetPlaylistItemMessage
import com.futo.fcast.receiver.models.SetSpeedMessage
import com.futo.fcast.receiver.models.SetVolumeMessage
import com.futo.fcast.receiver.models.SubscribeEventMessage
import com.futo.fcast.receiver.models.UnsubscribeEventMessage
import com.futo.fcast.receiver.models.VersionMessage
import com.futo.fcast.receiver.models.VolumeUpdateMessage
import com.futo.fcast.receiver.proxy.ProxyService
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import java.net.SocketAddress
import java.util.UUID

data class AppCache(
    var interfaces: Any? = null,
    // TODO: fix version name (currently 1.0.0)
    val appName: String = BuildConfig.VERSION_NAME,
    val appVersion: String = BuildConfig.VERSION_CODE.toString(),
    var playMessage: PlayMessage? = null,
    var playerVolume: Double? = null,
    var playbackUpdate: PlaybackUpdateMessage? = null,
    var subscribedKeys: Set<String> = setOf(),

    var playlistContent: PlaylistContent? = null,
)

class NetworkService : Service() {
    private var _discoveryService: DiscoveryService? = null
    private var _stopped = false
    private var _tcpListenerService: TcpListenerService? = null
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
        val descriptionText =
            "Listening on port ${TcpListenerService.PORT} (TCP) and port ${WebSocketListenerService.PORT} (Websocket)"

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val importance = NotificationManager.IMPORTANCE_DEFAULT
            val channel = NotificationChannel(CHANNEL_ID, name, importance).apply {
                description = descriptionText
            }

            val notificationManager: NotificationManager =
                getSystemService(NOTIFICATION_SERVICE) as NotificationManager
            notificationManager.createNotificationChannel(channel)
        }

        val notification: Notification = createNotificationBuilder()
            .setContentTitle(name)
            .setContentText(descriptionText)
            .setSmallIcon(R.drawable.ic_stat_name)
            .build()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }

        val onNewSession: (FCastSession) -> Unit = { session ->
            _scope?.launch(Dispatchers.Main) {
                Log.i(TAG, "On new session ${session.id}")

                withContext(Dispatchers.IO) {
                    try {
                        Log.i(TAG, "Sending version ${session.id}")
                        session.send(Opcode.Version, VersionMessage(PROTOCOL_VERSION))
                    } catch (e: Throwable) {
                        Log.e(TAG, "Failed to send version ${session.id}", e)
                    }
                }
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

        ConnectionMonitor(_scope!!)
        ProxyService().apply {
            start()
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

        try {
            _webSocketListenerService?.stop()
        } catch (_: Throwable) {
            //Ignored
        } finally {
            _webSocketListenerService = null
        }

        _scope?.cancel()
        _scope = null

        Toast.makeText(this, "Stopped FCast service", Toast.LENGTH_LONG).show()
        instance = null
    }

    private inline fun <reified T> send(opcode: Opcode, message: T) {
        val sender: (FCastSession) -> Unit = { session: FCastSession ->
            _scope?.launch(Dispatchers.IO) {
                try {
                    session.send(opcode, message)
                    Log.i(TAG, "Opcode sent (opcode = $opcode) ${session.id}")
                } catch (e: Throwable) {
                    Log.w(TAG, "Failed to send opcode (opcode = $opcode) ${session.id}", e)
                }
            }
        }

        _tcpListenerService?.forEachSession(sender)
        _webSocketListenerService?.forEachSession(sender)
    }

    suspend fun preparePlayMessage(
        message: PlayMessage,
        cachedPlayerVolume: Double?
    ): Pair<PlayMessage?, PlaylistContent?> {
        // Protocol v2 FCast PlayMessage does not contain volume field and could result in the receiver
        // getting out-of-sync with the sender when player windows are closed and re-opened. Volume
        // is cached in the play message when volume is not set in v3 PlayMessage.
        var rendererMessage = PlayMessage(
            message.container, message.url,
            message.content, message.time, message.volume ?: cachedPlayerVolume,
            message.speed, message.headers, message.metadata
        )

        rendererMessage = ProxyService.proxyPlayIfRequired(rendererMessage)

        if (message.container == "application/json") {
            val jsonStr: String =
                if (message.url != null) fetchJSON(message.url).toString() else message.content
                    ?: ""

            try {
                val json = Json.decodeFromString<ContentObject>(jsonStr)

                when (json.contentType) {
                    ContentType.Playlist -> {
                        val playlistContent = Json.decodeFromString<PlaylistContent>(jsonStr)
                        _mediaCache?.destroy()
                        _mediaCache = MediaCache(playlistContent)

                        cache.playlistContent = playlistContent
                        return Pair(null, playlistContent)
                    }
                }
            } catch (e: IllegalArgumentException) {
                Log.w(
                    com.futo.fcast.receiver.TAG,
                    "JSON format is not a supported format, attempting to render as text: error=$e"
                )
            }
        }

        cache.playMessage = rendererMessage
        return Pair(rendererMessage, null)
    }

    fun onPlay(playMessage: PlayMessage) {
        _scope?.launch(Dispatchers.IO) {
            send(Opcode.PlayUpdate, PlayUpdateMessage(System.currentTimeMillis(), playMessage))
            cache.playMessage = playMessage

            val messageInfo = withContext(Dispatchers.IO) {
                preparePlayMessage(playMessage, cache.playerVolume)
            }

            _scope?.launch(Dispatchers.Main) {
                try {
                    if (PlayerActivity.instance == null) {
                        val i = Intent(this@NetworkService, PlayerActivity::class.java)
                        i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)

                        if (activityCount > 0) {
                            startActivity(i)
                        } else if (Settings.canDrawOverlays(this@NetworkService)) {
                            val pi = PendingIntent.getActivity(
                                this@NetworkService,
                                0,
                                i,
                                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
                            )
                            pi.send()
                        } else {
                            val pi = PendingIntent.getActivity(
                                this@NetworkService,
                                0,
                                i,
                                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
                            )
                            val playNotification = createNotificationBuilder()
                                .setContentTitle("FCast")
                                .setContentText("New content received. Tap to play.")
                                .setSmallIcon(R.drawable.ic_stat_name)
                                .setContentIntent(pi)
                                .setPriority(NotificationCompat.PRIORITY_HIGH)
                                .setAutoCancel(true)
                                .build()

                            val notificationManager =
                                getSystemService(NOTIFICATION_SERVICE) as NotificationManager
                            notificationManager.notify(PLAY_NOTIFICATION_ID, playNotification)
                        }
                    } else {
                        if (playMessage.container == "application/json") {
                            PlayerActivity.instance?.onPlayPlaylist(messageInfo.second!!)
                        } else {
                            PlayerActivity.instance?.play(messageInfo.first!!)
                        }
                    }
                } catch (e: Throwable) {
                    Log.e(TAG, "Failed to play", e)
                }
            }
        }
    }

    fun onPause() {
        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.pause()
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to pause", e)
            }
        }
    }

    fun onResume() {
        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.resume()
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to resume", e)
            }
        }
    }

    fun onStop() {
        cache.playMessage = null
        cache.playlistContent = null
        cache.playbackUpdate = null

        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.finish()
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to stop", e)
            }
        }
    }

    fun onSeek(message: SeekMessage) {
        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.seek(message)
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to seek", e)
            }
        }
    }

    fun onSetVolume(message: SetVolumeMessage) {
        cache.playerVolume = message.volume

        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.setVolume(message)
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to seek", e)
            }
        }
    }

    fun onSetSpeed(message: SetSpeedMessage) {
        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.setSpeed(message)
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to seek", e)
            }
        }
    }

    fun onConnect(listener: ListenerService, sessionId: UUID, address: SocketAddress) {
        ConnectionMonitor.onConnect(listener, sessionId, address) {
            frontendConnections.add(sessionId)
        }
    }

    fun onDisconnect(sessionId: UUID, address: SocketAddress) {
        ConnectionMonitor.onDisconnect(sessionId, address) {
            frontendConnections.remove(sessionId)
        }
    }

    fun onPing(sessionId: UUID) {
        ConnectionMonitor.onPingPong(sessionId)
    }

    fun onPong(sessionId: UUID) {
        ConnectionMonitor.onPingPong(sessionId)
    }

    fun onVersion(message: VersionMessage) {
        Log.i(TAG, "Received 'Version' message from sender: $message")
    }

    fun onInitial(message: InitialSenderMessage) {
        Log.i(TAG, "Received 'Initial' message from sender: $message")
    }

    fun onSetPlaylistItem(message: SetPlaylistItemMessage) {
        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.setPlaylistItem(message.itemIndex)
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to set playlist item", e)
            }
        }
    }

    fun onSubscribeEvent(id: UUID, message: SubscribeEventMessage) {
        if (_tcpListenerService?.getSessions()?.contains(id) == true) {
            _tcpListenerService?.subscribeEvent(id, message.event)
        }
        if (_webSocketListenerService?.getSessions()?.contains(id) == true) {
            _webSocketListenerService?.subscribeEvent(id, message.event)
        }
    }

    fun onUnsubscribeEvent(id: UUID, message: UnsubscribeEventMessage) {
        if (_tcpListenerService?.getSessions()?.contains(id) == true) {
            _tcpListenerService?.unsubscribeEvent(id, message.event)
        }
        if (_webSocketListenerService?.getSessions()?.contains(id) == true) {
            _webSocketListenerService?.unsubscribeEvent(id, message.event)
        }
    }

    fun sendPlaybackError(error: String) {
        val message = PlaybackErrorMessage(error)
        send(Opcode.PlaybackError, message)
    }

    fun sendPlaybackUpdate(message: PlaybackUpdateMessage) {
        send(Opcode.PlaybackUpdate, message)
    }

    fun sendVolumeUpdate(value: VolumeUpdateMessage) {
        cache.playerVolume = value.volume
        send(Opcode.VolumeUpdate, value)
    }

    fun sendEvent(message: EventMessage) {
        _tcpListenerService?.send(Opcode.Event, message)
        _webSocketListenerService?.send(Opcode.Event, message)
    }

    fun playRequest(message: PlayMessage, playlistIndex: Int) {
        Log.d(TAG, "Received play request for index $playlistIndex: $message")
        val updatedMessage = if (_mediaCache?.has(playlistIndex) == true) {
            PlayMessage(
                message.container,
                _mediaCache?.getUrl(playlistIndex),
                message.content,
                message.time,
                message.volume,
                message.speed,
                message.headers,
                message.metadata
            )
        } else {
            message
        }

        _mediaCache?.cacheItems(playlistIndex)
        onPlay(updatedMessage)
    }

    fun getSubscribedKeys(): Pair<Set<String>, Set<String>> {
        val tcpListenerSubscribedKeys =
            _tcpListenerService?.getAllSubscribedKeys() ?: Pair(emptySet(), emptySet())
        val webSocketListenerSubscribedKeys =
            _webSocketListenerService?.getAllSubscribedKeys() ?: Pair(emptySet(), emptySet())
        val subscribeData = Pair(
            tcpListenerSubscribedKeys.first + webSocketListenerSubscribedKeys.first,
            tcpListenerSubscribedKeys.second + webSocketListenerSubscribedKeys.second
        )

        return subscribeData
    }

    companion object {
        private const val CHANNEL_ID = "NetworkListenerServiceChannel"
        private const val NOTIFICATION_ID = 1
        private const val PLAY_NOTIFICATION_ID = 2
        const val TAG = "NetworkService"
        var activityCount = 0
        var instance: NetworkService? = null

        val cache: AppCache = AppCache()
        private var _mediaCache: MediaCache? = null

        fun getPlayMessage(): PlayMessage? {
            return if (cache.playMessage == null) null else PlayMessage(
                cache.playMessage!!.container,
                cache.playMessage!!.url,
                cache.playMessage!!.content,
                cache.playbackUpdate!!.time,
                cache.playerVolume,
                cache.playbackUpdate!!.speed,
                cache.playMessage!!.headers,
                cache.playMessage!!.metadata
            )
        }
    }
}

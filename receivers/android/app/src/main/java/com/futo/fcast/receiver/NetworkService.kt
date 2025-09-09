package com.futo.fcast.receiver

import android.app.*
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
import com.futo.fcast.receiver.models.streamingMediaTypes
import kotlinx.coroutines.*
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
    var subscribedKeys: Set<String>,
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
        val descriptionText = "Listening on port ${TcpListenerService.PORT} (TCP) and port ${WebSocketListenerService.PORT} (Websocket)"

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val importance = NotificationManager.IMPORTANCE_DEFAULT
            val channel = NotificationChannel(CHANNEL_ID, name, importance).apply {
                description = descriptionText
            }

            val notificationManager: NotificationManager = getSystemService(NOTIFICATION_SERVICE) as NotificationManager
            notificationManager.createNotificationChannel(channel)
        }

        val notification: Notification = createNotificationBuilder()
            .setContentTitle(name)
            .setContentText(descriptionText)
            .setSmallIcon(R.drawable.ic_stat_name)
            .build()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
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

    suspend fun preparePlayMessage(message: PlayMessage, cachedPlayerVolume: Double?) {
        // Protocol v2 FCast PlayMessage does not contain volume field and could result in the receiver
        // getting out-of-sync with the sender when player windows are closed and re-opened. Volume
        // is cached in the play message when volume is not set in v3 PlayMessage.
        var rendererMessage = PlayMessage(
            message.container, message.url,
            message.content, message.time, message.volume ?: cachedPlayerVolume,
            message.speed, message.headers, message.metadata
        )

        rendererMessage = proxyPlayIfRequired(rendererMessage)

        if (message.container === "application/json") {
            val jsonStr: String = if (message.url != null) fetchJSON(message.url).toString() else message.content ?: ""

            try {
                val json = Json.decodeFromString<ContentObject>(jsonStr)

                when (json.contentType) {
                    ContentType.Playlist -> {
                        val playlistContent = Json.decodeFromString<PlaylistContent>(jsonStr)
                        _mediaCache?.destroy()
                        _mediaCache = MediaCache(playlistContent)

                        onPlayPlaylist(playlistContent)
                        return
                    }
                }
            }
            catch (e: IllegalArgumentException) {
                Log.w(com.futo.fcast.receiver.TAG, "JSON format is not a supported format, attempting to render as text: error=$e")
            }
        }

        onPlay(rendererMessage)
    }

    fun sendPlaybackError(error: String) {
        Log.i(TAG, "sendPlaybackError")
        val message = PlaybackErrorMessage(error)
        send(Opcode.PlaybackError, message)
    }

    fun sendPlaybackUpdate(message: PlaybackUpdateMessage) {
        Log.i(TAG, "sendPlaybackUpdate")
        send(Opcode.PlaybackUpdate, message)
    }

    fun sendCastVolumeUpdate(value: VolumeUpdateMessage) {
        Log.i(TAG, "sendCastVolumeUpdate")
        send(Opcode.VolumeUpdate, value)
    }

    fun onPlay(playMessage: PlayMessage) {
        Log.i(TAG, "onPlay")

        // TODO: update implementation to electron receiver
        cache.playMessage = playMessage

        _scope?.launch(Dispatchers.Main) {
            try {
                if (PlayerActivity.instance == null) {
                    val i = Intent(this@NetworkService, PlayerActivity::class.java)
                    i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)

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
                            .setSmallIcon(R.drawable.ic_stat_name)
                            .setContentIntent(pi)
                            .setPriority(NotificationCompat.PRIORITY_HIGH)
                            .setAutoCancel(true)
                            .build()

                        val notificationManager = getSystemService(NOTIFICATION_SERVICE) as NotificationManager
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

    fun onPlayPlaylist(message: PlaylistContent) {
        Log.i(TAG, "onPlayPlaylist: $message")

        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.onPlayPlaylist(message)
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to play playlist", e)
            }
        }
    }

    fun onPause() {
        Log.i(TAG, "onPause")

        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.pause()
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to pause", e)
            }
        }
    }

    fun onResume() {
        Log.i(TAG, "onResume")

        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.resume()
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to resume", e)
            }
        }
    }

    fun onStop() {
        Log.i(TAG, "onStop")

        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.finish()
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to stop", e)
            }
        }
    }

    fun onSeek(message: SeekMessage) {
        Log.i(TAG, "onSeek: $message")

        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.seek(message)
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to seek", e)
            }
        }
    }

    fun onSetVolume(message: SetVolumeMessage) {
        Log.i(TAG, "onSetVolume: $message")

        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.setVolume(message)
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to seek", e)
            }
        }
    }

    fun onSetSpeed(message: SetSpeedMessage) {
        Log.i(TAG, "setSpeedMessage: $message")

        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.setSpeed(message)
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to seek", e)
            }
        }
    }

    fun onVersion(message: VersionMessage) {
        Log.i(TAG, "onVersion")

        // implementation TBD
    }

    fun onPing(sessionId: UUID) {
        ConnectionMonitor.onPingPong(sessionId)
    }

    fun onPong(sessionId: UUID) {
        ConnectionMonitor.onPingPong(sessionId)
    }

    fun onInitial(message: InitialSenderMessage) {
        Log.i(TAG, "Received 'Initial' message from sender: $message")
    }

    fun onSetPlaylistItem(message: SetPlaylistItemMessage) {
        Log.i(TAG, "onSetPlaylistItem: $message")

        _scope?.launch(Dispatchers.Main) {
            try {
                PlayerActivity.instance?.setPlaylistItem(message.itemIndex)
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to set playlist item", e)
            }
        }
    }

    fun onSubscribeEvent(message: SubscribeEventMessage) {
        Log.i(TAG, "onSubscribeEvent")

        // implementation TBD
    }

    fun onUnsubscribeEvent(message: UnsubscribeEventMessage) {
        Log.i(TAG, "onUnsubscribeEvent")

        // implementation TBD
    }

    fun sendEvent(message: EventMessage) {
        Log.i(TAG, "sendEvent")
        _tcpListenerService?.send(Opcode.Event, message)
        _webSocketListenerService?.send(Opcode.Event, message)
    }

    // play-request


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

    companion object {
        private const val CHANNEL_ID = "NetworkListenerServiceChannel"
        private const val NOTIFICATION_ID = 1
        private const val PLAY_NOTIFICATION_ID = 2
        const val TAG = "NetworkService"
        var activityCount = 0
        var instance: NetworkService? = null

        val cache: AppCache = AppCache(null,BuildConfig.VERSION_NAME, BuildConfig.VERSION_CODE.toString(), null, null, setOf())
        private var _mediaCache: MediaCache? = null


        
        var key: String? = null
        var cert: String? = null
//        var proxyServer: http.Server
//        var proxyServerAddress: AddressInfo
        var proxiedFiles: MutableMap<String, PlayMessage> = mutableMapOf()

        private suspend fun setupProxyServer() {
//            try {
//                Log.i(TAG, "Proxy server starting")
//
//                val port = 0
//                proxyServer = http.createServer((req, res) => {
//                    Log.i(TAG, "Request received")
//                    val requestUrl = "http://${req.headers.host}${req.url}"
//
//                    val proxyInfo = proxiedFiles[requestUrl]
//
//                    if (!proxyInfo) {
//                        res.writeHead(404)
//                        res.end('Not found')
//                        return
//                    }
//
//                    if (proxyInfo.url.startsWith('app://')) {
//                        var start: number = 0
//                        var end: number = null
//                        val contentSize = MediaCache.getInstance().getObjectSize(proxyInfo.url)
//                        if (req.headers.range) {
//                            val range = req.headers.range.slice(6).split('-')
//                            start = (range.length > 0) ? parseInt(range[0]) : 0
//                            end = (range.length > 1) ? parseInt(range[1]) : null
//                        }
//
//                        Log.d(TAG, "Fetching byte range from cache: start=${start}, end=${end}")
//                        val stream = MediaCache.getInstance().getObject(proxyInfo.url, start, end)
//                        var responseCode = null
//                        var responseHeaders = null
//
//                        if (start != 0) {
//                            responseCode = 206
//                            responseHeaders = {
//                                'Accept-Ranges': 'bytes',
//                                'Content-Length': contentSize - start,
//                                'Content-Range': `bytes ${start}-${end ? end : contentSize - 1}/${contentSize}`,
//                                'Content-Type': proxyInfo.container,
//                            }
//                        }
//                        else {
//                            responseCode = 200
//                            responseHeaders = {
//                                'Accept-Ranges': 'bytes',
//                                'Content-Length': contentSize,
//                                'Content-Type': proxyInfo.container,
//                            }
//                        }
//
//                        Log.d(TAG,"Serving content ${proxyInfo.url} with response headers: $responseHeaders")
//                        res.writeHead(responseCode, responseHeaders)
//                        stream.pipe(res)
//                    }
//                    else {
//                        val omitHeaders = setOf(
//                            "host",
//                            "connection",
//                            "keep-alive",
//                            "proxy-authenticate",
//                            "proxy-authorization",
//                            "te",
//                            "trailers",
//                            "transfer-encoding",
//                            "upgrade",
//                        )
//
//                        val filteredHeaders = Object.fromEntries(Object.entries(req.headers)
//                            .filter(([key]) => !omitHeaders.has(key.toLowerCase()))
//                        .map(([key, value]) => [key, Array.isArray(value) ? value.join(', ') : value]))
//
//                        val parsedUrl = url.parse(proxyInfo.url)
//                        val options: http.RequestOptions = {
//                            ... parsedUrl,
//                            method: req.method,
//                            headers: { ...filteredHeaders, ...proxyInfo.headers }
//                        }
//
//                        val proxyReq = http.request(options, (proxyRes) => {
//                            res.writeHead(proxyRes.statusCode, proxyRes.headers)
//                            proxyRes.pipe(res, { end: true })
//                        })
//
//                        req.pipe(proxyReq, { end: true })
//                        proxyReq.on('error', (e) => {
//                            Log.e(TAG, "Problem with request: ${e.message}")
//                            res.writeHead(500)
//                            res.end()
//                        })
//                    }
//                })
//                NetworkService.proxyServer.on('error', e => {
//                    reject(e)
//                })
//                NetworkService.proxyServer.listen(port, '127.0.0.1', () => {
//                    proxyServerAddress = proxyServer.address() as AddressInfo
//                    Log.i(TAG, "Proxy server running at http://127.0.0.1:${proxyServerAddress.port}/")
//                    resolve()
//                })
//            } catch (e) {
//                reject(e)
//            }
        }

        suspend fun proxyPlayIfRequired(message: PlayMessage): PlayMessage {
            if (message.url !== null && (message.url.startsWith("app://") || (message.headers !== null && !streamingMediaTypes.contains(message.container.lowercase())))) {
                return PlayMessage(
                    message.container, proxyFile(message),
                    message.content, message.time, message.volume,
                    message.speed, message.headers
                )
            }
            return message
        }

        suspend fun proxyFile(message: PlayMessage): String {
            val proxiedUrl = "TEMP"
//            if (!proxyServer) {
//                await NetworkService.setupProxyServer()
//            }
//
//            val proxiedUrl = "http://127.0.0.1:${proxyServerAddress.port}/${UUID.randomUUID()}"
            Log.i(TAG, "Proxied url $proxiedUrl, $message")
            proxiedFiles[proxiedUrl] = message
            return proxiedUrl
        }
    }
}

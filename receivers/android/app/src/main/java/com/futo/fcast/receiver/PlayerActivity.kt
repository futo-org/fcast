package com.futo.fcast.receiver

import android.annotation.SuppressLint
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.view.KeyEvent
import android.view.WindowInsets
import android.view.WindowManager
import androidx.activity.compose.setContent
import androidx.annotation.OptIn
import androidx.appcompat.app.AppCompatActivity
import androidx.core.net.toUri
import androidx.lifecycle.lifecycleScope
import androidx.media3.common.MediaItem
import androidx.media3.common.MediaMetadata
import androidx.media3.common.MediaMetadata.MEDIA_TYPE_MIXED
import androidx.media3.common.MediaMetadata.MEDIA_TYPE_MUSIC
import androidx.media3.common.MediaMetadata.MEDIA_TYPE_VIDEO
import androidx.media3.common.PlaybackException
import androidx.media3.common.PlaybackParameters
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.DefaultDataSource
import androidx.media3.datasource.DefaultHttpDataSource
import androidx.media3.datasource.HttpDataSource
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.dash.DashMediaSource
import androidx.media3.exoplayer.hls.HlsMediaSource
import androidx.media3.exoplayer.source.DefaultMediaSourceFactory
import androidx.media3.exoplayer.trackselection.DefaultTrackSelector
import com.futo.fcast.receiver.models.ControlFocus
import com.futo.fcast.receiver.models.EventMessage
import com.futo.fcast.receiver.models.EventType
import com.futo.fcast.receiver.models.GenericMediaMetadata
import com.futo.fcast.receiver.models.MediaItemEvent
import com.futo.fcast.receiver.models.PlayMessage
import com.futo.fcast.receiver.models.PlaybackState
import com.futo.fcast.receiver.models.PlaybackUpdateMessage
import com.futo.fcast.receiver.models.PlayerActivityViewModel
import com.futo.fcast.receiver.models.PlaylistContent
import com.futo.fcast.receiver.models.SeekMessage
import com.futo.fcast.receiver.models.SetSpeedMessage
import com.futo.fcast.receiver.models.SetVolumeMessage
import com.futo.fcast.receiver.models.VolumeUpdateMessage
import com.futo.fcast.receiver.models.streamingMediaTypes
import com.futo.fcast.receiver.models.supportedAudioTypes
import com.futo.fcast.receiver.models.supportedVideoTypes
import com.futo.fcast.receiver.views.PlayerActivity
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import java.io.File
import java.io.FileOutputStream
import kotlin.math.abs
import kotlin.math.floor
import kotlin.math.max
import kotlin.math.min

enum class ControlBarMode {
    KeyboardMouse,
    Remote
}

class PlayerActivity : AppCompatActivity() {
    private lateinit var _exoPlayer: ExoPlayer
    private var _shouldPlaybackRestartOnConnectivity: Boolean = false
    private lateinit var _connectivityManager: ConnectivityManager
    private var _wasPlaying = false

    val viewModel = PlayerActivityViewModel()
    private var _lastPlayerUpdateGenerationTime: Long = 0

    private var _cachedPlaylist: PlaylistContent = PlaylistContent(items = arrayListOf())
    private var _cachedPlayMediaItem: com.futo.fcast.receiver.models.MediaItem =
        com.futo.fcast.receiver.models.MediaItem("")
    private var _playlistIndex: Int = 0
    private var _isMediaItem: Boolean = false
    private var _playItemCached = false

    private val _uiHideTimer = Timer({
        if (!_exoPlayer.isPlaying) {
            _controlMode = ControlBarMode.KeyboardMouse
            viewModel.controlFocus = ControlFocus.None
            viewModel.showControls = false
        }
    }, 5000)
    private val _showDurationTimer = Timer(::mediaEndHandler, 0, false)

    private var _controlMode = ControlBarMode.KeyboardMouse

    private val _connectivityEvents = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            super.onAvailable(network)
            Log.i(TAG, "_connectivityEvents onAvailable")

            try {
                lifecycleScope.launch(Dispatchers.Main) {
                    Log.i(TAG, "onConnectionAvailable")

                    val pos = _exoPlayer.currentPosition
                    val dur = _exoPlayer.duration
                    if (_shouldPlaybackRestartOnConnectivity && abs(pos - dur) > 2000) {
                        Log.i(
                            TAG,
                            "Playback ended due to connection loss, resuming playback since connection is restored."
                        )
                        _exoPlayer.playWhenReady = true
                        _exoPlayer.prepare()
                        _exoPlayer.play()
                    }
                }
            } catch (ex: Throwable) {
                Log.w(TAG, "Failed to handle connection available event", ex)
            }
        }
    }

    private val _playerEventListener = object : Player.Listener {
        override fun onPlaybackStateChanged(playbackState: Int) {
            super.onPlaybackStateChanged(playbackState)
            Log.i(TAG, "onPlaybackStateChanged playbackState=$playbackState")

            if (_shouldPlaybackRestartOnConnectivity && playbackState == ExoPlayer.STATE_READY) {
                Log.i(TAG, "_shouldPlaybackRestartOnConnectivity=false")
                _shouldPlaybackRestartOnConnectivity = false
            }

//            if (playbackState == ExoPlayer.STATE_READY) {
//                setStatus(false, null)
//            } else if (playbackState == ExoPlayer.STATE_BUFFERING) {
//                setStatus(true, null)
//            }

            if (playbackState == ExoPlayer.STATE_READY || playbackState == ExoPlayer.STATE_BUFFERING) {
                viewModel.statusMessage = null
            }

            sendPlaybackUpdate()
        }

        override fun onPlayWhenReadyChanged(playWhenReady: Boolean, reason: Int) {
            super.onPlayWhenReadyChanged(playWhenReady, reason)
            sendPlaybackUpdate()
        }

        override fun onPositionDiscontinuity(
            oldPosition: Player.PositionInfo,
            newPosition: Player.PositionInfo,
            reason: Int
        ) {
            super.onPositionDiscontinuity(oldPosition, newPosition, reason)
            sendPlaybackUpdate()
        }

        override fun onPlayerError(error: PlaybackException) {
            super.onPlayerError(error)

            Log.e(TAG, "onPlayerError: $error")

            when (error.errorCode) {
                PlaybackException.ERROR_CODE_IO_BAD_HTTP_STATUS,
                PlaybackException.ERROR_CODE_IO_CLEARTEXT_NOT_PERMITTED,
                PlaybackException.ERROR_CODE_IO_FILE_NOT_FOUND,
                PlaybackException.ERROR_CODE_IO_INVALID_HTTP_CONTENT_TYPE,
                PlaybackException.ERROR_CODE_IO_NETWORK_CONNECTION_FAILED,
                PlaybackException.ERROR_CODE_IO_NETWORK_CONNECTION_TIMEOUT,
                PlaybackException.ERROR_CODE_IO_NO_PERMISSION,
                PlaybackException.ERROR_CODE_IO_READ_POSITION_OUT_OF_RANGE,
                PlaybackException.ERROR_CODE_IO_UNSPECIFIED -> {
                    Log.i(TAG, "IO error, set _shouldPlaybackRestartOnConnectivity=true")
                    _shouldPlaybackRestartOnConnectivity = true
                }
            }

            val fullMessage = getFullExceptionMessage(error)
//            setStatus(false, fullMessage)
            viewModel.statusMessage = fullMessage

            lifecycleScope.launch(Dispatchers.IO) {
                try {
                    NetworkService.instance?.sendPlaybackUpdate(
                        PlaybackUpdateMessage(
                            System.currentTimeMillis(),
                            0,
                            0.0,
                            0.0,
                            0.0
                        )
                    )
                    NetworkService.instance?.sendPlaybackError(fullMessage)
                } catch (e: Throwable) {
                    Log.e(TAG, "Unhandled error sending playback error", e)
                }
            }
        }

        override fun onVolumeChanged(volume: Float) {
            super.onVolumeChanged(volume)
            lifecycleScope.launch(Dispatchers.IO) {
                try {
                    NetworkService.instance?.sendVolumeUpdate(
                        VolumeUpdateMessage(
                            System.currentTimeMillis(),
                            volume.toDouble()
                        )
                    )
                } catch (e: Throwable) {
                    Log.e(TAG, "Unhandled error sending volume update", e)
                }
            }
        }

        override fun onPlaybackParametersChanged(playbackParameters: PlaybackParameters) {
            super.onPlaybackParametersChanged(playbackParameters)
            sendPlaybackUpdate()
        }
    }

    fun sendPlaybackUpdate() {
        val state: PlaybackState
        when (_exoPlayer.playbackState) {
            ExoPlayer.STATE_READY -> {
                state = if (_exoPlayer.playWhenReady) {
                    PlaybackState.Playing
                } else {
                    PlaybackState.Paused
                }
            }

            ExoPlayer.STATE_BUFFERING -> {
                state = if (_exoPlayer.playWhenReady) {
                    PlaybackState.Playing
                } else {
                    PlaybackState.Paused
                }
            }

            ExoPlayer.STATE_ENDED -> {
                state = PlaybackState.Paused
            }

            else -> {
                state = PlaybackState.Idle
            }
        }

        var time: Double? = null
        var duration: Double? = null
        var speed: Double? = null
        if (state != PlaybackState.Idle) {
            duration = (_exoPlayer.duration / 1000.0).coerceAtLeast(0.0)
            time = (_exoPlayer.currentPosition / 1000.0).coerceAtLeast(0.0).coerceAtMost(duration)
            speed = _exoPlayer.playbackParameters.speed.toDouble().coerceAtLeast(0.01)
        }

        val updateMessage = PlaybackUpdateMessage(
            System.currentTimeMillis(),
            state.value.toInt(),
            time,
            duration,
            speed,
            if (_isMediaItem) _playlistIndex else null
        )
        NetworkService.cache.playbackUpdate = updateMessage

        if (updateMessage.generationTime > _lastPlayerUpdateGenerationTime) {
            _lastPlayerUpdateGenerationTime = updateMessage.generationTime
            lifecycleScope.launch(Dispatchers.IO) {
                try {
                    NetworkService.instance?.sendPlaybackUpdate(updateMessage)
                } catch (e: Throwable) {
                    Log.e(TAG, "Unhandled error sending playback update", e)
                }
            }
        }
    }

    @OptIn(UnstableApi::class)
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Log.i(TAG, "onCreate")

        val trackSelector = DefaultTrackSelector(this)
        trackSelector.parameters = trackSelector.parameters
            .buildUpon()
            .setPreferredTextLanguage("df")
            .setSelectUndeterminedTextLanguage(true)
            .build()

        _exoPlayer = ExoPlayer.Builder(this)
            .setTrackSelector(trackSelector).build()
        _exoPlayer.addListener(_playerEventListener)


        setContent {
            PlayerActivity(viewModel, _exoPlayer)
        }

//        setContentView(R.layout.activity_player)
        setFullScreen()

//        setStatus(true, null)
        viewModel.statusMessage = null

//        val trackSelector = DefaultTrackSelector(this)
//        trackSelector.parameters = trackSelector.parameters
//            .buildUpon()
//            .setPreferredTextLanguage("df")
//            .setSelectUndeterminedTextLanguage(true)
//            .build()
//
//        _exoPlayer = ExoPlayer.Builder(this)
//            .setTrackSelector(trackSelector).build()
//        _exoPlayer.addListener(_playerEventListener)
//
        Log.i(TAG, "Attached onConnectionAvailable listener.")
        _connectivityManager = getSystemService(CONNECTIVITY_SERVICE) as ConnectivityManager
        val netReq = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
            .addTransportType(NetworkCapabilities.TRANSPORT_ETHERNET)
            .addTransportType(NetworkCapabilities.TRANSPORT_CELLULAR)
            .build()
        _connectivityManager.registerNetworkCallback(netReq, _connectivityEvents)

//        val playMessage = intent.getStringExtra("message")?.let {
//            try {
//                Json.decodeFromString<PlayMessage>(it)
//            } catch (e: Throwable) {
//                Log.i(TAG, "Failed to deserialize play message.", e)
//                null
//            }
//        }
//        playMessage?.let { play(it) }

        NetworkService.cache.playlistContent?.also {
            onPlayPlaylist(it)
        } ?: run {
            NetworkService.cache.playMessage?.let { play(it) }
        }

        instance = this
        NetworkService.activityCount++

//        lifecycleScope.launch(Dispatchers.Main) {
//            while (lifecycleScope.isActive) {
//                try {
//                    sendPlaybackUpdate()
//                    delay(1000)
//                } catch (e: Throwable) {
//                    Log.e(TAG, "Failed to send playback update.", e)
//                }
//            }
//        }
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) setFullScreen()
    }

    private fun getFullExceptionMessage(ex: Throwable): String {
        val messages = mutableListOf<String>()
        var current: Throwable? = ex
        while (current != null) {
            messages.add(current.message ?: "Unknown error")
            current = current.cause
        }
        return messages.joinToString(separator = " → ")
    }

//    private fun setStatus(isLoading: Boolean, message: String?) {
////        PlayerActivityViewModel.isLoading.value = isLoading
////        PlayerActivityViewModel.statusMessage.value = message
//        viewModel.statusMessage = message
//
//
////
//    }

    private fun setFullScreen() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window.insetsController?.hide(WindowInsets.Type.statusBars())
            window.insetsController?.hide(WindowInsets.Type.navigationBars())
            window.insetsController?.hide(WindowInsets.Type.systemBars())
        } else {
            @Suppress("DEPRECATION")
            window.setFlags(
                WindowManager.LayoutParams.FLAG_FULLSCREEN,
                WindowManager.LayoutParams.FLAG_FULLSCREEN
            )
        }
    }

    private fun setControlMode(mode: ControlBarMode, immediateHide: Boolean = true) {
        if (mode == ControlBarMode.KeyboardMouse) {
            _uiHideTimer.enable()

            if (immediateHide) {
                viewModel.controlFocus = ControlFocus.None
                viewModel.showControls = false
            } else {
                _uiHideTimer.start()
            }
        } else {
            viewModel.controlFocus = ControlFocus.ProgressBar
            viewModel.showControls = true
            _uiHideTimer.start()
        }

        _controlMode = mode
    }

    private val minSkipInterval = 10

    private var skipBackRepeat = false
    private var skipBackInterval = minSkipInterval
    private var skipBackIntervalIncrease = false
    private val skipBackTimer = Timer({ skipBackIntervalIncrease = true }, 2000, false)

    private var skipForwardRepeat = false
    private var skipForwardInterval = minSkipInterval
    private var skipForwardIntervalIncrease = false
    private val skipForwardTimer = Timer({ skipForwardIntervalIncrease = true }, 2000, false)

    private fun skipBack(repeat: Boolean = false) {
        if (!skipBackRepeat && repeat) {
            skipBackRepeat = true
            skipBackTimer.start()
        } else if (skipBackRepeat && skipBackIntervalIncrease && repeat) {
            skipBackInterval = if (skipBackInterval == 10) 30 else min(skipBackInterval + 30, 300)
            skipBackIntervalIncrease = false
            skipBackTimer.start()
        } else if (!repeat) {
            skipBackTimer.stop()
            skipBackRepeat = false
            skipBackIntervalIncrease = false
            skipBackInterval = minSkipInterval
        }

        _exoPlayer.seekTo(max(_exoPlayer.currentPosition - (skipBackInterval * 1000), 0))
    }

    private fun skipForward(repeat: Boolean = false) {
        if (!skipForwardRepeat && repeat) {
            skipForwardRepeat = true
            skipForwardTimer.start()
        } else if (skipForwardRepeat && skipForwardIntervalIncrease && repeat) {
            skipForwardInterval =
                if (skipForwardInterval == 10) 30 else min(skipForwardInterval + 30, 300)
            skipForwardIntervalIncrease = false
            skipForwardTimer.start()
        } else if (!repeat) {
            skipForwardTimer.stop()
            skipForwardRepeat = false
            skipForwardIntervalIncrease = false
            skipForwardInterval = minSkipInterval
        }

        // todo: handle livestreams
//        if (!isLivePosition) {
        _exoPlayer.seekTo(
            min(
                _exoPlayer.currentPosition + (skipForwardInterval * 1000),
                _exoPlayer.duration
            )
        )
//        }
    }

    override fun onPause() {
        super.onPause()

        _wasPlaying = _exoPlayer.isPlaying
        _exoPlayer.pause()
    }

    override fun onResume() {
        super.onResume()
        if (_wasPlaying) {
            _exoPlayer.play()
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        Log.i(TAG, "onDestroy")

        instance = null
        _connectivityManager.unregisterNetworkCallback(_connectivityEvents)
        _exoPlayer.removeListener(_playerEventListener)
        _exoPlayer.stop()
        NetworkService.activityCount--

        GlobalScope.launch(Dispatchers.IO) {
            try {
                NetworkService.instance?.sendPlaybackUpdate(
                    PlaybackUpdateMessage(
                        System.currentTimeMillis(),
                        0,
                        0.0,
                        0.0,
                        0.0
                    )
                )
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to send playback update.", e)
            }
        }
    }

    @SuppressLint("GestureBackNavigation")
    @OptIn(UnstableApi::class)
    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
//        Log.d(TAG, "KeyEvent: label=${event.displayLabel}, event=$event")
        var handledCase = false
        var key = event.displayLabel.toString()

        if (event.action == KeyEvent.ACTION_DOWN) {
            when (event.keyCode) {
                KeyEvent.KEYCODE_K,
                KeyEvent.KEYCODE_SPACE,
                KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE -> {
                    if (!_exoPlayer.isPlaying) {
                        resume()
                    } else {
                        pause()
                    }

                    handledCase = true
                }

                KeyEvent.KEYCODE_ENTER,
                KeyEvent.KEYCODE_DPAD_CENTER -> {
                    if (_controlMode == ControlBarMode.KeyboardMouse) {
                        setControlMode(ControlBarMode.Remote)
                    } else {
                        if (viewModel.controlFocus == ControlFocus.ProgressBar || viewModel.controlFocus == ControlFocus.Action) {
                            // Play/pause toggle
                            if (!_exoPlayer.isPlaying) {
                                resume()
                            } else {
                                pause()
                            }
                        } else if (viewModel.controlFocus == ControlFocus.PlayPrevious) {
                            previousPlaylistItem()
                        } else if (viewModel.controlFocus == ControlFocus.PlayNext) {
                            nextPlaylistItem()
                        }
                    }

                    key = "Enter"
                    handledCase = true
                }

                KeyEvent.KEYCODE_DPAD_UP -> {
                    if (_controlMode == ControlBarMode.KeyboardMouse) {
                        setControlMode(ControlBarMode.Remote)
                    } else {
                        if (viewModel.controlFocus == ControlFocus.ProgressBar) {
                            setControlMode(ControlBarMode.KeyboardMouse)
                        } else {
                            viewModel.controlFocus = ControlFocus.ProgressBar
                        }
                    }

                    key = "ArrowUp"
                    handledCase = true
                }

                KeyEvent.KEYCODE_DPAD_DOWN -> {
                    if (_controlMode == ControlBarMode.KeyboardMouse) {
                        setControlMode(ControlBarMode.Remote)
                    } else {
                        if (viewModel.controlFocus == ControlFocus.ProgressBar) {
                            viewModel.controlFocus = ControlFocus.Action
                        } else {
                            setControlMode(ControlBarMode.KeyboardMouse)
                        }
                    }

                    key = "ArrowDown"
                    handledCase = true
                }

                KeyEvent.KEYCODE_DPAD_LEFT -> {
                    if (_controlMode == ControlBarMode.KeyboardMouse) {
                        setControlMode(ControlBarMode.Remote)
                    } else {
                        if (viewModel.controlFocus == ControlFocus.ProgressBar || !_isMediaItem) {
                            skipBack(event.repeatCount > 0)
                        } else {
                            if (viewModel.controlFocus == ControlFocus.Action) {
                                viewModel.controlFocus = ControlFocus.PlayPrevious
                            } else if (viewModel.controlFocus == ControlFocus.PlayNext) {
                                viewModel.controlFocus = ControlFocus.Action
                            }
                        }
                    }

                    key = "ArrowLeft"
                    handledCase = true
                }

                KeyEvent.KEYCODE_DPAD_RIGHT -> {
                    if (_controlMode == ControlBarMode.KeyboardMouse) {
                        setControlMode(ControlBarMode.Remote)
                    } else {
                        if (viewModel.controlFocus == ControlFocus.ProgressBar || !_isMediaItem) {
                            skipForward(event.repeatCount > 0)
                        } else {
                            if (viewModel.controlFocus == ControlFocus.Action) {
                                viewModel.controlFocus = ControlFocus.PlayNext
                            } else if (viewModel.controlFocus == ControlFocus.PlayPrevious) {
                                viewModel.controlFocus = ControlFocus.Action
                            }
                        }
                    }

                    key = "ArrowRight"
                    handledCase = true
                }

                KeyEvent.KEYCODE_MEDIA_STOP -> {
//            window.parent.webOSApp.loadPage('main_window/index.html');
                    key = "Stop"
                    handledCase = true
                }

                KeyEvent.KEYCODE_MEDIA_REWIND -> {
                    skipBack(event.repeatCount > 0)
                    key = "Rewind"
                    handledCase = true
                }

                KeyEvent.KEYCODE_MEDIA_PLAY -> {
                    if (!_exoPlayer.isPlaying) {
                        resume()
                    }

                    key = "Play"
                    handledCase = true
                }

                KeyEvent.KEYCODE_MEDIA_PAUSE -> {
                    if (_exoPlayer.isPlaying) {
                        pause()
                    }

                    key = "Pause"
                    handledCase = true
                }

                KeyEvent.KEYCODE_MEDIA_FAST_FORWARD -> {
                    skipForward(event.repeatCount > 0);
                    key = "FastForward"
                    handledCase = true
                }

                KeyEvent.KEYCODE_BACK -> key = "Back"
            }
        }

        if (NetworkService.instance?.getSubscribedKeys()?.first?.contains(key) == true) {
            NetworkService.instance?.sendEvent(
                EventMessage(
                    System.currentTimeMillis(),
                    com.futo.fcast.receiver.models.KeyEvent(
                        EventType.KeyDown,
                        key,
                        event.repeatCount > 0,
                        handledCase
                    )
                )
            )
        }
        if (NetworkService.instance?.getSubscribedKeys()?.second?.contains(key) == true) {
            NetworkService.instance?.sendEvent(
                EventMessage(
                    System.currentTimeMillis(),
                    com.futo.fcast.receiver.models.KeyEvent(
                        EventType.KeyUp,
                        key,
                        event.repeatCount > 0,
                        handledCase
                    )
                )
            )
        }

        if (handledCase) {
            return true
        }

        return super.dispatchKeyEvent(event)
    }

    @OptIn(UnstableApi::class)
    fun play(playMessage: PlayMessage) {
        if (!_playItemCached) {
            _cachedPlayMediaItem = mediaItemFromPlayMessage(playMessage)
            _isMediaItem = false
        }
        NetworkService.instance?.sendEvent(
            EventMessage(
                System.currentTimeMillis(),
                MediaItemEvent(EventType.MediaItemChange, _cachedPlayMediaItem)
            )
        )
        Log.i(TAG, "Media playback changed: $_cachedPlayMediaItem")
        _playItemCached = false
        _showDurationTimer.stop()

//        if (player) {
//            if ((player.getSource() === value.url) || (player.getSource() === value.content)) {
//                if (value.time) {
//                    console.info('Skipped changing video URL because URL is the same. Discarding time and using current receiver time instead');
//                }
//                return;
//            }
//
//            player.destroy();
//            player = null;
//        }

        // todo finish electron implementation review
        viewModel.isLoading = true
        viewModel.isIdle = false
        viewModel.playMessage = playMessage
        sendPlaybackUpdate()
//        _playerPrevTime = 0
        _lastPlayerUpdateGenerationTime = 0
//        _isLive = false
//        _isLivePosition = false

        val mediaItemBuilder = MediaItem.Builder()
        if (playMessage.container.isNotEmpty()) {
            mediaItemBuilder.setMimeType(playMessage.container)
        }

        if (!playMessage.url.isNullOrEmpty()) {
            mediaItemBuilder.setUri(playMessage.url.toUri())
//            mediaItemBuilder.setUri(playMessage.url.toUri()).setImageDurationMs(10000)
        } else if (!playMessage.content.isNullOrEmpty()) {
            val tempFile = File.createTempFile("content_", ".tmp", cacheDir)
            tempFile.deleteOnExit()
            FileOutputStream(tempFile).use { output ->
                output.bufferedWriter().use { writer ->
                    writer.write(playMessage.content)
                }
            }

            mediaItemBuilder.setUri(Uri.fromFile(tempFile))
        } else {
            throw IllegalArgumentException("Either URL or content must be provided.")
        }

        val dataSourceFactory = if (playMessage.headers != null) {
            val httpDataSourceFactory: HttpDataSource.Factory = DefaultHttpDataSource.Factory()
            httpDataSourceFactory.setDefaultRequestProperties(playMessage.headers)
            DefaultDataSource.Factory(this, httpDataSourceFactory)

        } else {
            DefaultDataSource.Factory(this)
        }


        val mediaMetadataBuilder = MediaMetadata.Builder()

        if ((playMessage.metadata as? GenericMediaMetadata)?.title != null) {
            mediaMetadataBuilder.setTitle(playMessage.metadata.title)
        }
        if ((playMessage.metadata as? GenericMediaMetadata)?.thumbnailUrl != null) {
            mediaMetadataBuilder.setArtworkUri(playMessage.metadata.thumbnailUrl?.toUri())
        }
        if (streamingMediaTypes.contains(playMessage.container) || supportedVideoTypes.contains(
                playMessage.container
            )
        ) {
            mediaMetadataBuilder.setMediaType(MEDIA_TYPE_VIDEO)
        } else if (supportedAudioTypes.contains(playMessage.container)) {
            mediaMetadataBuilder.setMediaType(MEDIA_TYPE_MUSIC)
        } else {
            mediaMetadataBuilder.setMediaType(MEDIA_TYPE_MIXED)
        }

//        MEDIA_TYPE_MUSIC
//        mediaMetadataBuilder.setMediaType(MEDIA_TYPE_VIDEO)

        mediaItemBuilder.setMediaMetadata(mediaMetadataBuilder.build())

        val mediaItem = mediaItemBuilder.build()
        val mediaSource = when (playMessage.container) {
            "application/dash+xml" -> DashMediaSource.Factory(dataSourceFactory)
                .createMediaSource(mediaItem)

            "application/x-mpegurl" -> HlsMediaSource.Factory(dataSourceFactory)
                .createMediaSource(mediaItem)

            "application/vnd.apple.mpegurl" -> HlsMediaSource.Factory(dataSourceFactory)
                .createMediaSource(mediaItem)

            else -> DefaultMediaSourceFactory(dataSourceFactory).createMediaSource(mediaItem)
        }

        _exoPlayer.setMediaSource(mediaSource)
        _exoPlayer.setPlaybackSpeed(playMessage.speed?.toFloat() ?: 1.0f)

        if (playMessage.time != null) {
            _exoPlayer.seekTo((playMessage.time * 1000).toLong())
        }

//        setStatus(true, null)
        viewModel.statusMessage = null
        _wasPlaying = false
        _exoPlayer.playWhenReady = true
        _exoPlayer.prepare()
        _exoPlayer.play()
    }

    fun pause() {
        _exoPlayer.pause()
    }

    fun resume() {
        if (_exoPlayer.playbackState == ExoPlayer.STATE_ENDED && _exoPlayer.duration - _exoPlayer.currentPosition < 1000) {
            _exoPlayer.seekTo(0)
        }

        _exoPlayer.play()
    }

    fun seek(seekMessage: SeekMessage) {
        _exoPlayer.seekTo((seekMessage.time * 1000.0).toLong())
    }

    fun setVolume(setVolumeMessage: SetVolumeMessage) {
        _exoPlayer.volume = setVolumeMessage.volume.toFloat()
    }

    fun setSpeed(setSpeedMessage: SetSpeedMessage) {
        _exoPlayer.setPlaybackSpeed(setSpeedMessage.speed.toFloat())
    }

    fun onPlayPlaylist(message: PlaylistContent) {
        Log.i(TAG, "Handle play playlist message $message")
        _cachedPlaylist = message

        val offset = message.offset ?: 0
        val volume = message.items[offset].volume ?: message.volume
        val speed = message.items[offset].speed ?: message.speed
        val playMessage = PlayMessage(
            message.items[offset].container,
            message.items[offset].url,
            message.items[offset].content,
            message.items[offset].time,
            volume,
            speed,
            message.items[offset].headers,
            message.items[offset].metadata
        );

        _playlistIndex = offset
        _isMediaItem = true
        _cachedPlayMediaItem = message.items[offset]
        _playItemCached = true
        NetworkService.instance?.playRequest(playMessage, _playlistIndex)
    }

    fun setPlaylistItem(index: Int) {
        if (index >= 0 && index < _cachedPlaylist.items.size) {
            Log.i(TAG, "Setting playlist item to index $index")
            _playlistIndex = index
            _cachedPlayMediaItem = _cachedPlaylist.items[_playlistIndex]
            _playItemCached = true
            sendPlaybackUpdate()
            NetworkService.instance?.playRequest(
                playMessageFromMediaItem(_cachedPlaylist.items[_playlistIndex]),
                _playlistIndex
            )
            _showDurationTimer.stop()
        } else {
            Log.w(TAG, "Playlist index out of bounds $index, ignoring...")
        }
    }

    fun nextPlaylistItem() {
        setPlaylistItem(_playlistIndex + 1)
    }

    fun previousPlaylistItem() {
        setPlaylistItem(_playlistIndex - 1)
    }

    fun mediaPlayHandler() {
        if (viewModel.isLoading) {
            Log.i(TAG, "Media playback start: $_cachedPlayMediaItem")
            NetworkService.instance?.sendEvent(
                EventMessage(
                    System.currentTimeMillis(),
                    MediaItemEvent(EventType.MediaItemStart, _cachedPlayMediaItem)
                )
            )
            viewModel.isLoading = false
            viewModel.isIdle = false
            // TODO: thumbnail display

            if (_isMediaItem && _cachedPlayMediaItem.showDuration != null && _cachedPlayMediaItem.showDuration!! > 0) {
                _showDurationTimer.start((_cachedPlayMediaItem.showDuration!! * 1000).toLong())
            }
        } else {
            _showDurationTimer.resume()
        }

        sendPlaybackUpdate()
    }

    fun mediaEndHandler() {
        _showDurationTimer.stop()

        if (_isMediaItem) {
            _playlistIndex++

            if (_playlistIndex < _cachedPlaylist.items.size) {
                _cachedPlayMediaItem = _cachedPlaylist.items[_playlistIndex]
                _playItemCached = true
                NetworkService.instance?.playRequest(
                    playMessageFromMediaItem(_cachedPlaylist.items[_playlistIndex]),
                    _playlistIndex
                )
            } else {
                Log.i(TAG, "End of playlist: $_cachedPlayMediaItem")
                sendPlaybackUpdate()
                NetworkService.instance?.sendEvent(
                    EventMessage(
                        System.currentTimeMillis(),
                        MediaItemEvent(EventType.MediaItemEnd, _cachedPlayMediaItem)
                    )
                )

//                player.setAutoPlay(false);
                viewModel.isIdle = true
                _exoPlayer.stop()
            }
        } else {
            Log.i(TAG, "Media playback ended: $_cachedPlayMediaItem")
            sendPlaybackUpdate()
            NetworkService.instance?.sendEvent(
                EventMessage(
                    System.currentTimeMillis(),
                    MediaItemEvent(EventType.MediaItemEnd, _cachedPlayMediaItem)
                )
            )

//            player.setAutoPlay(false);
            viewModel.isIdle = true
            _exoPlayer.stop()
        }
    }

    companion object {
        var instance: PlayerActivity? = null
        const val TAG = "PlayerActivity"

        private const val SEEK_BACKWARD_MILLIS = 10_000
        private const val SEEK_FORWARD_MILLIS = 10_000

        @SuppressLint("DefaultLocale")
        fun formatDuration(duration: Long): String {
            if (duration < 0) {
                return "00:00"
            }

            val totalSeconds = floor(duration.toDouble() / 1000)
            val hours = floor(totalSeconds / 3600).toLong()
            val minutes = floor((totalSeconds % 3600) / 60).toLong()
            val seconds = floor(totalSeconds % 60).toLong()

            val paddedMinutes = minutes.toString().padStart(2, '0')
            val paddedSeconds = seconds.toString().padStart(2, '0')

            return if (hours > 0) {
                "$hours:$paddedMinutes:$paddedSeconds"
            } else {
                "$paddedMinutes:$paddedSeconds"
            }
        }
    }
}
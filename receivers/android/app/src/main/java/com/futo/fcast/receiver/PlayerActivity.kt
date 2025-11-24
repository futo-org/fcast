package com.futo.fcast.receiver

import android.annotation.SuppressLint
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
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.DefaultDataSource
import androidx.media3.datasource.DefaultHttpDataSource
import androidx.media3.datasource.HttpDataSource
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.dash.DashMediaSource
import androidx.media3.exoplayer.hls.HlsMediaSource
import androidx.media3.exoplayer.source.DefaultMediaSourceFactory
import androidx.media3.exoplayer.source.preload.DefaultPreloadManager
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
import com.futo.fcast.receiver.models.PlayerSource
import com.futo.fcast.receiver.models.SeekMessage
import com.futo.fcast.receiver.models.SetSpeedMessage
import com.futo.fcast.receiver.models.SetVolumeMessage
import com.futo.fcast.receiver.models.VolumeUpdateMessage
import com.futo.fcast.receiver.models.streamingMediaTypes
import com.futo.fcast.receiver.models.supportedAudioTypes
import com.futo.fcast.receiver.models.supportedImageTypes
import com.futo.fcast.receiver.models.supportedVideoTypes
import com.futo.fcast.receiver.views.PlayerActivity
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.MainScope
import kotlinx.coroutines.launch
import org.webrtc.EglBase
import org.webrtc.VideoTrack
import java.io.File
import java.io.FileOutputStream
import java.util.Locale
import java.util.UUID
import kotlin.math.abs
import kotlin.math.floor
import kotlin.math.max
import kotlin.math.min


enum class ControlBarMode {
    KeyboardMouse,
    Remote
}

@UnstableApi
class PlayerActivity : AppCompatActivity() {
    private lateinit var _exoPlayer: ExoPlayer

    private var _shouldPlaybackRestartOnConnectivity: Boolean = false
    private var _preloadManager: DefaultPreloadManager? = null
    private var _preloadMediaControl: MediaPreloadStatusControl? = null
    private var _wasPlaying = false

    private var _lastPlayerUpdateGenerationTime: Long = 0
    private var _isPlaylist: Boolean = false
    private var _usingPreloader: Boolean = false
    private var _cachedPlayMediaItem: com.futo.fcast.receiver.models.MediaItem =
        com.futo.fcast.receiver.models.MediaItem("")
//    private var _playlistIndex: Int = 0

    val viewModel = PlayerActivityViewModel()

    private val _uiHideTimer = Timer({
        if (_exoPlayer.isPlaying) {
            _controlMode = ControlBarMode.KeyboardMouse
            viewModel.controlFocus = ControlFocus.None
            viewModel.showControls = false
        }
    }, 5000)
    private val _showDurationTimer = Timer(::mediaEndHandler, 0, false)
    private var _controlMode = ControlBarMode.Remote

    private val _playerEventListener = object : Player.Listener {
        override fun onPlaybackStateChanged(playbackState: Int) {
            super.onPlaybackStateChanged(playbackState)
            Log.i(TAG, "onPlaybackStateChanged playbackState=$playbackState")

            if (_shouldPlaybackRestartOnConnectivity && playbackState == ExoPlayer.STATE_READY) {
                Log.i(TAG, "_shouldPlaybackRestartOnConnectivity=false")
                _shouldPlaybackRestartOnConnectivity = false
            }

            if (playbackState == ExoPlayer.STATE_READY || playbackState == ExoPlayer.STATE_BUFFERING) {
                viewModel.errorMessage = null
            }

            sendPlaybackUpdate()
            updateKeepScreenOnFlag()
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
            viewModel.showControls = false
            viewModel.errorMessage = fullMessage

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
    }

    @OptIn(UnstableApi::class)
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Log.i(TAG, "onCreate")
        initializeExoPlayer()

        setContent {
            PlayerActivity(viewModel)
        }

        setFullScreen()
        viewModel.errorMessage = null

        instance = this
        NetworkService.activityCount++
        NetworkService.cache.playMessage?.let { play(it) }
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) setFullScreen()
    }

    override fun onPause() {
        super.onPause()

        _uiHideTimer.stop()
        _wasPlaying = _exoPlayer.isPlaying
        _exoPlayer.pause()
    }

    override fun onResume() {
        super.onResume()
        if (_wasPlaying) {
            _uiHideTimer.start()
            _exoPlayer.play()
        }
    }

    fun clearViewModelSource() {
        try {
            val source = viewModel.source
            when (source) {
                is PlayerSource.Whep -> {
                    source.client.disconnect()
                    source.videoTrack.value = null
                }

                else -> {}
            }
            viewModel.source = null
        } catch (e: Exception) {
            Log.e(TAG, "WHEP client failed to disconnect: $e")
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        Log.i(TAG, "onDestroy")

        _exoPlayer.removeListener(_playerEventListener)
        _exoPlayer.stop()
        NetworkService.activityCount--

        clearViewModelSource()
        viewModel.source = null

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

    override fun finish() {
        instance = null
        viewModel.isIdle = true
        super.finish()
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
                    playPauseToggle()
                    handledCase = true
                }

                KeyEvent.KEYCODE_ENTER,
                KeyEvent.KEYCODE_DPAD_CENTER -> {
                    if (_controlMode == ControlBarMode.KeyboardMouse) {
                        setControlMode(ControlBarMode.Remote)
                    } else {
                        when (viewModel.controlFocus) {
                            ControlFocus.None -> {
                                if (!viewModel.showControls) {
                                    setControlMode(ControlBarMode.Remote)
                                }
                            }

                            ControlFocus.ProgressBar, ControlFocus.Action -> playPauseToggle()
                            ControlFocus.PlayPrevious -> _exoPlayer.seekToPreviousMediaItem()
                            ControlFocus.PlayNext -> _exoPlayer.seekToNextMediaItem()
                            ControlFocus.SeekForward -> _exoPlayer.seekTo(
                                min(
                                    _exoPlayer.currentPosition + (skipForwardInterval * 1000),
                                    _exoPlayer.duration
                                )
                            )

                            ControlFocus.SeekBackward -> _exoPlayer.seekTo(
                                max(
                                    _exoPlayer.currentPosition - (skipBackInterval * 1000),
                                    0
                                )
                            )
                        }

                        _uiHideTimer.restart()
                    }

                    key = "Enter"
                    handledCase = true
                }

                KeyEvent.KEYCODE_DPAD_UP -> {
                    if (_controlMode == ControlBarMode.KeyboardMouse) {
                        setControlMode(ControlBarMode.Remote)
                    } else {
                        when (viewModel.controlFocus) {
                            ControlFocus.None -> {
                                if (!viewModel.showControls) {
                                    setControlMode(ControlBarMode.Remote)
                                }
                            }

                            ControlFocus.ProgressBar -> setControlMode(ControlBarMode.KeyboardMouse)
                            else -> {
                                if (_exoPlayer.mediaMetadata.mediaType != MEDIA_TYPE_MIXED) {
                                    viewModel.controlFocus = ControlFocus.ProgressBar
                                } else {
                                    setControlMode(ControlBarMode.KeyboardMouse)
                                }
                            }
                        }

                        _uiHideTimer.restart()
                    }

                    key = "ArrowUp"
                    handledCase = true
                }

                KeyEvent.KEYCODE_DPAD_DOWN -> {
                    if (_controlMode == ControlBarMode.KeyboardMouse) {
                        setControlMode(ControlBarMode.Remote)
                    } else {
                        when (viewModel.controlFocus) {
                            ControlFocus.None -> {
                                if (!viewModel.showControls) {
                                    setControlMode(ControlBarMode.Remote)
                                }
                            }

                            ControlFocus.ProgressBar -> {
                                if (_exoPlayer.mediaMetadata.mediaType != MEDIA_TYPE_MIXED || viewModel.hasDuration) {
                                    viewModel.controlFocus = ControlFocus.Action
                                } else {
                                    viewModel.controlFocus = ControlFocus.SeekForward
                                }
                            }

                            else -> setControlMode(ControlBarMode.KeyboardMouse)
                        }

                        _uiHideTimer.restart()
                    }

                    key = "ArrowDown"
                    handledCase = true
                }

                KeyEvent.KEYCODE_DPAD_LEFT -> {
                    if (!viewModel.showControls && _exoPlayer.mediaMetadata.mediaType == MEDIA_TYPE_MIXED && _isPlaylist) {
                        setPlaylistItem(_exoPlayer.previousMediaItemIndex)
                    } else if (_controlMode == ControlBarMode.KeyboardMouse) {
                        setControlMode(ControlBarMode.Remote)
                    } else {
                        if (viewModel.controlFocus == ControlFocus.ProgressBar) {
                            skipBack(event.repeatCount > 0)
                        } else {
                            if (_exoPlayer.mediaMetadata.mediaType != MEDIA_TYPE_MIXED || viewModel.hasDuration) {
                                if (viewModel.controlFocus == ControlFocus.PlayNext || viewModel.controlFocus == ControlFocus.SeekForward) {
                                    viewModel.controlFocus = ControlFocus.Action
                                } else {
                                    viewModel.controlFocus =
                                        if (_isPlaylist) ControlFocus.PlayPrevious else ControlFocus.SeekBackward
                                }
                            } else {
                                viewModel.controlFocus = ControlFocus.PlayPrevious
                            }
                        }

                        _uiHideTimer.restart()
                    }

                    key = "ArrowLeft"
                    handledCase = true
                }

                KeyEvent.KEYCODE_DPAD_RIGHT -> {
                    if (!viewModel.showControls && _exoPlayer.mediaMetadata.mediaType == MEDIA_TYPE_MIXED && _isPlaylist) {
                        setPlaylistItem(_exoPlayer.nextMediaItemIndex)
                    } else if (_controlMode == ControlBarMode.KeyboardMouse) {
                        setControlMode(ControlBarMode.Remote)
                    } else {
                        if (viewModel.controlFocus == ControlFocus.ProgressBar) {
                            skipForward(event.repeatCount > 0)
                        } else {
                            if (_exoPlayer.mediaMetadata.mediaType != MEDIA_TYPE_MIXED || viewModel.hasDuration) {
                                if (viewModel.controlFocus == ControlFocus.PlayPrevious || viewModel.controlFocus == ControlFocus.SeekBackward) {
                                    viewModel.controlFocus = ControlFocus.Action
                                } else {
                                    viewModel.controlFocus =
                                        if (_isPlaylist) ControlFocus.PlayNext else ControlFocus.SeekForward
                                }
                            } else {
                                viewModel.controlFocus = ControlFocus.PlayNext
                            }
                        }

                        _uiHideTimer.restart()
                    }

                    key = "ArrowRight"
                    handledCase = true
                }

                KeyEvent.KEYCODE_MEDIA_STOP -> {
                    finish()
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

                KeyEvent.KEYCODE_BACK -> {
                    finish()
                    key = "Back"
                    handledCase = true
                }
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

    private fun initializeExoPlayer(usePreloader: Boolean = false) {
        val trackSelector = DefaultTrackSelector(this)
        trackSelector.parameters = trackSelector.parameters
            .buildUpon()
            .setPreferredTextLanguages(Locale.getDefault().language, "en", "df")
            .setSelectUndeterminedTextLanguage(true)
//            .setViewportSizeToPhysicalDisplaySize(true)
//            .setMinVideoSize(0, 0)
//            .setMaxVideoSize(Int.MAX_VALUE, Int.MAX_VALUE)
            .build()

        val exoPlayerBuilder = ExoPlayer.Builder(this)
            .setTrackSelector(trackSelector)
            .setSeekForwardIncrementMs(SEEK_FORWARD_MILLIS)
            .setSeekBackIncrementMs(SEEK_BACKWARD_MILLIS)

        _exoPlayer = if (usePreloader) {
            if (_preloadManager != null) {
                _preloadMediaControl = null
                _preloadManager!!.release()
                _exoPlayer.removeListener(_playerEventListener)
                _exoPlayer.release()
            }

            _preloadMediaControl = MediaPreloadStatusControl(NetworkService.cache.playlistContent!!)
            val preloadManagerBuilder = DefaultPreloadManager.Builder(this, _preloadMediaControl!!)
            _preloadManager = preloadManagerBuilder.build()
            preloadManagerBuilder.buildExoPlayer(exoPlayerBuilder)
        } else {
            exoPlayerBuilder.build()
        }

//        _exoPlayer.preloadConfiguration = ExoPlayer.PreloadConfiguration(5_000_000L)
        _exoPlayer.addListener(_playerEventListener)
        _exoPlayer.playWhenReady = true

//        _exoPlayer.setvideoscalingMode
//        _exoPlayer.videoScalingMode = C.VIDEO_SCALING_MODE_SCALE_TO_FIT_WITH_CROPPING

        viewModel.source = PlayerSource.Exo(_exoPlayer)
    }

    private fun onMediaLoad(message: PlayMessage, playlistIndex: Int) {
        _exoPlayer.setPlaybackSpeed(viewModel.playMessage?.speed?.toFloat() ?: 1.0f)

        if (message.volume != null && message.volume >= 0 && message.volume <= 1) {
            _exoPlayer.volume = message.volume.toFloat()
        } else {
            // Protocol v2 FCast PlayMessage does not contain volume field and could result in the receiver
            // getting out-of-sync with the sender on 1st playback.
            _exoPlayer.volume = 1f
            NetworkService.instance?.sendVolumeUpdate(
                VolumeUpdateMessage(
                    System.currentTimeMillis(),
                    1.toDouble()
                )
            )
        }

        mediaPlayHandler()

        if (_isPlaylist) {
            _exoPlayer.seekTo(playlistIndex, 0)
        }

        if (message.time != null) {
            _exoPlayer.seekTo((message.time * 1000).toLong())
        }

        _exoPlayer.prepare()
        _exoPlayer.play()
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
        if (viewModel.errorMessage != null) {
            return
        }

        if (mode == ControlBarMode.KeyboardMouse) {
            _uiHideTimer.enable()

            if (immediateHide) {
                viewModel.controlFocus = ControlFocus.None
                viewModel.showControls = false
            } else {
                _uiHideTimer.start()
            }
        } else {
            if (_exoPlayer.mediaMetadata.mediaType != MEDIA_TYPE_MIXED) {
                viewModel.controlFocus = ControlFocus.ProgressBar
            } else if (viewModel.hasDuration) {
                viewModel.controlFocus = ControlFocus.Action
            } else {
                viewModel.controlFocus = ControlFocus.PlayNext
            }

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

        _exoPlayer.seekTo(
            min(
                _exoPlayer.currentPosition + (skipForwardInterval * 1000),
                _exoPlayer.duration
            )
        )
    }

    fun onNetworkConnectionAvailable() {
        try {
            lifecycleScope.launch(Dispatchers.Main) {
                Log.i(TAG, "onNetworkConnectionAvailable")

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

    fun updateKeepScreenOnFlag() {
        if (_exoPlayer.playWhenReady && (_exoPlayer.playbackState == Player.STATE_READY || _exoPlayer.playbackState == Player.STATE_BUFFERING)) {
            window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        } else {
            window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
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
            if (_isPlaylist) _exoPlayer.currentMediaItemIndex else null
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
    fun play(playMessage: PlayMessage) {
        pendingPlay = false

        if ((viewModel.playMessage?.url != null && viewModel.playMessage?.url == playMessage.url) || (viewModel.playMessage?.content != null && viewModel.playMessage?.content == playMessage.content)) {
            if (playMessage.time != null) {
                Log.i(
                    TAG,
                    "Skipped changing video URL because URL is the same. Discarding time and using current receiver time instead"
                )
            }
            return
        }

        _isPlaylist = NetworkService.cache.playlistContent != null
        val playlistOffset =
            if (_isPlaylist) NetworkService.cache.playlistContent!!.offset ?: 0 else 0
        _usingPreloader = _isPlaylist &&
                ((NetworkService.cache.playlistContent!!.forwardCache != null && NetworkService.cache.playlistContent!!.forwardCache!! > 0) ||
                        (NetworkService.cache.playlistContent!!.backwardCache != null && NetworkService.cache.playlistContent!!.backwardCache!! > 0))

        val message = if (_isPlaylist) {
            val offset = NetworkService.cache.playlistContent!!.offset ?: 0
            val volume =
                NetworkService.cache.playlistContent!!.items[offset].volume ?: playMessage.volume
            val speed =
                NetworkService.cache.playlistContent!!.items[offset].speed ?: playMessage.speed

            PlayMessage(
                NetworkService.cache.playlistContent!!.items[offset].container,
                NetworkService.cache.playlistContent!!.items[offset].url,
                NetworkService.cache.playlistContent!!.items[offset].content,
                NetworkService.cache.playlistContent!!.items[offset].time,
                volume,
                speed,
                NetworkService.cache.playlistContent!!.items[offset].headers,
                NetworkService.cache.playlistContent!!.items[offset].metadata
            )
        } else {
            playMessage
        }

        _cachedPlayMediaItem = if (_isPlaylist) {
            NetworkService.cache.playlistContent!!.items[NetworkService.cache.playlistContent!!.offset
                ?: 0]
        } else {
            mediaItemFromPlayMessage(message)
        }

        NetworkService.instance?.sendEvent(
            EventMessage(
                System.currentTimeMillis(),
                MediaItemEvent(EventType.MediaItemChange, _cachedPlayMediaItem)
            )
        )
        Log.i(TAG, "Media playback changed: $_cachedPlayMediaItem")
        _showDurationTimer.stop()

        viewModel.isLoading = true
        viewModel.isIdle = false
        viewModel.playMessage = message
        sendPlaybackUpdate()
        _lastPlayerUpdateGenerationTime = 0

        clearViewModelSource()

        if (!_isPlaylist && _cachedPlayMediaItem.container == "application/x-whep") {
            val client = WhepClient(this, eglBase)

            client.addTrackListener(object : ClientBaseListener {
                override fun onTrackAdded(track: VideoTrack) {
                    Log.i("SurfaceViewRenderer", "Video track was added")
                    val source = viewModel.source
                    when (source) {
                        is PlayerSource.Whep -> {
                            source.videoTrack.value = track
                        }

                        else -> {}
                    }
                }
            })

            val url = _cachedPlayMediaItem.url ?: return
            MainScope().launch {
                client.connect(url)
            }

            Log.i(TAG, "Starting WHEP playback with: $_cachedPlayMediaItem")
            NetworkService.instance?.sendEvent(
                EventMessage(
                    System.currentTimeMillis(),
                    MediaItemEvent(EventType.MediaItemStart, _cachedPlayMediaItem)
                )
            )

            viewModel.isLoading = false
            viewModel.isIdle = false
            viewModel.hasDuration = false
            viewModel.source = PlayerSource.Whep(client)
            viewModel.errorMessage = null
            viewModel.hasDuration = false
            _wasPlaying = false

            sendPlaybackUpdate()
            onMediaLoad(message, 0)

            return
        }

        if (_usingPreloader) {
            initializeExoPlayer(true)
        }

        _exoPlayer.clearMediaItems()
        val mediaItemList =
            if (_isPlaylist) NetworkService.cache.playlistContent!!.items else arrayListOf(
                _cachedPlayMediaItem
            )
        mediaItemList.forEachIndexed { index, item ->
            val mediaMetadataBuilder = MediaMetadata.Builder()

            if ((message.metadata as? GenericMediaMetadata)?.title != null) {
                mediaMetadataBuilder.setTitle(message.metadata.title)
            }
            if ((message.metadata as? GenericMediaMetadata)?.thumbnailUrl != null) {
                mediaMetadataBuilder.setArtworkUri(message.metadata.thumbnailUrl?.toUri())
            }

            val mediaType = when {
                streamingMediaTypes.contains(message.container) || supportedVideoTypes.contains(
                    message.container
                ) -> MEDIA_TYPE_VIDEO

                supportedAudioTypes.contains(message.container) -> MEDIA_TYPE_MUSIC
                else -> MEDIA_TYPE_MIXED
            }
            mediaMetadataBuilder.setMediaType(mediaType)

            val mediaItemBuilder = MediaItem.Builder()
            mediaItemBuilder.setMediaMetadata(mediaMetadataBuilder.build())

            if (item.container.isNotEmpty()) {
                mediaItemBuilder.setMimeType(message.container)
            }

            if (!item.url.isNullOrEmpty()) {
                mediaItemBuilder.setUri(item.url.toUri())
            } else if (!item.content.isNullOrEmpty()) {
                val tempFile = File.createTempFile("content_", ".tmp", cacheDir)
                tempFile.deleteOnExit()
                FileOutputStream(tempFile).use { output ->
                    output.bufferedWriter().use { writer ->
                        writer.write(message.content)
                    }
                }

                mediaItemBuilder.setUri(Uri.fromFile(tempFile))
            } else {
                throw IllegalArgumentException("Either URL or content must be provided.")
            }

            if (mediaType == MEDIA_TYPE_MIXED) {
                if (item.showDuration != null && item.showDuration > 0) {
                    mediaItemBuilder.setImageDurationMs(item.showDuration.toLong() * 1000)
                } else {
                    // values in the range of Long.MAX_VALUE causes bugs with exoplayer
                    mediaItemBuilder.setImageDurationMs(Int.MAX_VALUE.toLong() * 16)
                }
            }

            val dataSourceFactory = if (item.headers != null) {
                val httpDataSourceFactory: HttpDataSource.Factory = DefaultHttpDataSource.Factory()
                httpDataSourceFactory.setDefaultRequestProperties(item.headers)
                DefaultDataSource.Factory(this, httpDataSourceFactory)

            } else {
                DefaultDataSource.Factory(this)
            }

            mediaItemBuilder.setMediaId(UUID.randomUUID().toString())
            val mediaItem = mediaItemBuilder.build()
            val mediaSource = when (item.container) {
                "application/dash+xml" -> DashMediaSource.Factory(dataSourceFactory)
                    .createMediaSource(mediaItem)

                "application/x-mpegurl" -> HlsMediaSource.Factory(dataSourceFactory)
                    .createMediaSource(mediaItem)

                "application/vnd.apple.mpegurl" -> HlsMediaSource.Factory(dataSourceFactory)
                    .createMediaSource(mediaItem)

                else -> DefaultMediaSourceFactory(dataSourceFactory).createMediaSource(mediaItem)
            }

            if (_usingPreloader) {
                _preloadManager?.add(mediaSource, index)
                _exoPlayer.addMediaSource(_preloadManager?.getMediaSource(mediaItem)!!)
            } else {
                _exoPlayer.addMediaSource(mediaSource)
            }
        }

        if (_usingPreloader) {
            _preloadMediaControl?.currentItemIndex = playlistOffset
            _preloadManager?.setCurrentPlayingIndex(playlistOffset)
            _preloadManager?.invalidate()
        }

        if (playlistOffset != 0) {
            _exoPlayer.seekTo(playlistOffset, (message.time?.times(1000))?.toLong() ?: 0)
        }

        onMediaLoad(message, playlistOffset)
        viewModel.errorMessage = null
        viewModel.hasDuration = true
        _wasPlaying = false
        viewModel.source = PlayerSource.Exo(_exoPlayer)
//        _exoPlayer.playWhenReady = true
//        _exoPlayer.prepare()
    }

    fun playPauseToggle() {
        if (!_exoPlayer.isPlaying) {
            resume()
        } else {
            pause()
        }
    }

    fun pause() {
        _uiHideTimer.stop()
        _exoPlayer.pause()
    }

    fun resume() {
        if (_exoPlayer.playbackState == ExoPlayer.STATE_ENDED && _exoPlayer.duration - _exoPlayer.currentPosition < 1000) {
            _exoPlayer.seekTo(0)
        } else if (viewModel.isIdle) {
            _exoPlayer.seekTo(0)
            _exoPlayer.prepare()
            mediaPlayHandler()
        }

        _uiHideTimer.start()
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

    fun setPlaylistItem(index: Int) {
        if (index >= 0 && index < _exoPlayer.mediaItemCount) {
            _showDurationTimer.stop()
            Log.i(TAG, "Setting playlist item to index $index")

            _cachedPlayMediaItem = NetworkService.cache.playlistContent!!.items[index]
            NetworkService.instance?.sendEvent(
                EventMessage(
                    System.currentTimeMillis(),
                    MediaItemEvent(EventType.MediaItemChange, _cachedPlayMediaItem)
                )
            )
            Log.i(TAG, "Media playback changed: $_cachedPlayMediaItem")

            if (_usingPreloader) {
                _preloadMediaControl?.currentItemIndex = index
                _preloadManager?.setCurrentPlayingIndex(index)
                _preloadManager?.invalidate()
            }

            onMediaLoad(playMessageFromMediaItem(_cachedPlayMediaItem), index)
            sendPlaybackUpdate()
        } else {
            Log.w(TAG, "Playlist index out of bounds $index, ignoring...")
        }
    }

    fun mediaPlayHandler() {
        Log.i(TAG, "Media playback start: $_cachedPlayMediaItem")
        NetworkService.instance?.sendEvent(
            EventMessage(
                System.currentTimeMillis(),
                MediaItemEvent(EventType.MediaItemStart, _cachedPlayMediaItem)
            )
        )
        viewModel.isLoading = false
        viewModel.isIdle = false
        viewModel.hasDuration =
            (_cachedPlayMediaItem.showDuration != null && _cachedPlayMediaItem.showDuration!! > 0) || _exoPlayer.mediaMetadata.mediaType != MEDIA_TYPE_MIXED

        if (_isPlaylist && _cachedPlayMediaItem.showDuration != null && _cachedPlayMediaItem.showDuration!! > 0) {
            if (!supportedImageTypes.contains(_cachedPlayMediaItem.container)) {
                _showDurationTimer.start((_cachedPlayMediaItem.showDuration!! * 1000).toLong())
            }
        }

        sendPlaybackUpdate()
    }

    fun mediaEndHandler() {
        if (!viewModel.isIdle) {
            _showDurationTimer.stop()

            if (_isPlaylist) {
                if (_exoPlayer.currentMediaItemIndex < _exoPlayer.mediaItemCount) {
                    Log.i(TAG, "Media playback ended: $_cachedPlayMediaItem")
                    setPlaylistItem(_exoPlayer.nextMediaItemIndex)
                } else {
                    Log.i(TAG, "End of playlist: $_cachedPlayMediaItem")
                    sendPlaybackUpdate()
                    NetworkService.instance?.sendEvent(
                        EventMessage(
                            System.currentTimeMillis(),
                            MediaItemEvent(EventType.MediaItemEnd, _cachedPlayMediaItem)
                        )
                    )

                    viewModel.isIdle = true
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

                viewModel.isIdle = true
            }
        }
    }

    fun uiHideControlsTimerStateChange() {
        if (viewModel.showControls) {
            _uiHideTimer.start()
        }
    }

    fun saveArtworkDataToUri(artworkData: ByteArray, fileName: String): Uri? {
        try {
            val cacheFile = File(this.cacheDir, fileName)
            FileOutputStream(cacheFile).use { fos ->
                fos.write(artworkData)
            }

            return Uri.fromFile(cacheFile)
        } catch (e: Throwable) {
            Log.e(TAG, "Error creating artwork uri", e)
            return null
        }
    }

    companion object {
        var instance: PlayerActivity? = null
        var pendingPlay = false
        const val TAG = "PlayerActivity"
        val eglBase = EglBase.create()

        private const val SEEK_BACKWARD_MILLIS = 10_000L
        private const val SEEK_FORWARD_MILLIS = 10_000L

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

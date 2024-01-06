package com.futo.fcast.receiver

import android.content.Context
import android.graphics.drawable.Animatable
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.view.KeyEvent
import android.view.View
import android.view.WindowInsets
import android.view.WindowManager
import android.widget.ImageView
import android.widget.TextView
import androidx.annotation.OptIn
import androidx.appcompat.app.AppCompatActivity
import androidx.constraintlayout.widget.ConstraintLayout
import androidx.lifecycle.lifecycleScope
import androidx.media3.common.MediaItem
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
import androidx.media3.ui.PlayerView
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.serialization.json.Json
import java.io.File
import java.io.FileOutputStream
import kotlin.math.abs
import kotlin.math.max


class PlayerActivity : AppCompatActivity() {
    private lateinit var _playerControlView: PlayerView
    private lateinit var _imageSpinner: ImageView
    private lateinit var _textMessage: TextView
    private lateinit var _layoutOverlay: ConstraintLayout
    private lateinit var _exoPlayer: ExoPlayer
    private var _shouldPlaybackRestartOnConnectivity: Boolean = false
    private lateinit var _connectivityManager: ConnectivityManager
    private var _wasPlaying = false

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
                        Log.i(TAG, "Playback ended due to connection loss, resuming playback since connection is restored.")
                        _exoPlayer.playWhenReady = true
                        _exoPlayer.prepare()
                        _exoPlayer.play()
                    }
                }
            } catch(ex: Throwable) {
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

            if (playbackState == ExoPlayer.STATE_READY) {
                setStatus(false, null)
            } else if (playbackState == ExoPlayer.STATE_BUFFERING) {
                setStatus(true, null)
            }

            sendPlaybackUpdate()
        }

        override fun onPlayWhenReadyChanged(playWhenReady: Boolean, reason: Int) {
            super.onPlayWhenReadyChanged(playWhenReady, reason)
            sendPlaybackUpdate()
        }

        override fun onPositionDiscontinuity(oldPosition: Player.PositionInfo, newPosition: Player.PositionInfo, reason: Int) {
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
            setStatus(false, fullMessage)

            lifecycleScope.launch(Dispatchers.IO) {
                try {
                    NetworkService.instance?.sendPlaybackUpdate(PlaybackUpdateMessage(
                        System.currentTimeMillis(),
                        0.0,
                        0.0,
                        0,
                        0.0
                    ))
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
                    NetworkService.instance?.sendCastVolumeUpdate(VolumeUpdateMessage(System.currentTimeMillis(), volume.toDouble()))
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

    private fun sendPlaybackUpdate() {
        val state: Int
        if (_exoPlayer.playbackState == ExoPlayer.STATE_READY) {
            if (_exoPlayer.playWhenReady) {
                state = 1

            } else {
                state = 2
            }
        } else if (_exoPlayer.playbackState == ExoPlayer.STATE_BUFFERING) {
            if (_exoPlayer.playWhenReady) {
                state = 1
            } else {
                state = 2
            }
        } else {
            state = 0
        }

        val time: Double
        val duration: Double
        val speed: Double
        if (state != 0) {
            duration = (_exoPlayer.duration / 1000.0).coerceAtLeast(1.0)
            time = (_exoPlayer.currentPosition / 1000.0).coerceAtLeast(0.0).coerceAtMost(duration)
            speed = _exoPlayer.playbackParameters.speed.toDouble().coerceAtLeast(0.01)
        } else {
            time = 0.0
            duration = 0.0
            speed = 1.0
        }

        val playbackUpdate = PlaybackUpdateMessage(
            System.currentTimeMillis(),
            time,
            duration,
            state,
            speed
        )

        lifecycleScope.launch(Dispatchers.IO) {
            try {
                NetworkService.instance?.sendPlaybackUpdate(playbackUpdate)
            } catch (e: Throwable) {
                Log.e(TAG, "Unhandled error sending playback update", e)
            }
        }
    }

    @OptIn(UnstableApi::class)
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Log.i(TAG, "onCreate")

        setContentView(R.layout.activity_player)
        setFullScreen()

        _playerControlView = findViewById(R.id.player_control_view)
        _imageSpinner = findViewById(R.id.image_spinner)
        _textMessage = findViewById(R.id.text_message)
        _layoutOverlay = findViewById(R.id.layout_overlay)

        setStatus(true, null)

        val trackSelector = DefaultTrackSelector(this)
        trackSelector.parameters = trackSelector.parameters
            .buildUpon()
            .setPreferredTextLanguage("df")
            .setSelectUndeterminedTextLanguage(true)
            .build()

        _exoPlayer = ExoPlayer.Builder(this)
            .setTrackSelector(trackSelector).build()
        _exoPlayer.addListener(_playerEventListener)
        _playerControlView.player = _exoPlayer
        _playerControlView.controllerAutoShow = false

        Log.i(TAG, "Attached onConnectionAvailable listener.")
        _connectivityManager = getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        val netReq = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
            .addTransportType(NetworkCapabilities.TRANSPORT_ETHERNET)
            .addTransportType(NetworkCapabilities.TRANSPORT_CELLULAR)
            .build()
        _connectivityManager.registerNetworkCallback(netReq, _connectivityEvents)

        val playMessage = intent.getStringExtra("message")?.let {
            try {
                Json.decodeFromString<PlayMessage>(it)
            } catch (e: Throwable) {
                Log.i(TAG, "Failed to deserialize play message.", e)
                null
            }
        }
        playMessage?.let { play(it) }

        instance = this
        NetworkService.activityCount++

        lifecycleScope.launch(Dispatchers.Main) {
            while (lifecycleScope.isActive) {
                try {
                    sendPlaybackUpdate()
                    delay(1000)
                } catch (e: Throwable) {
                    Log.e(TAG, "Failed to send playback update.", e)
                }
            }
        }
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

    private fun setStatus(isLoading: Boolean, message: String?) {
        if (isLoading) {
            (_imageSpinner.drawable as Animatable?)?.start()
            _imageSpinner.visibility = View.VISIBLE
        } else {
            (_imageSpinner.drawable as Animatable?)?.stop()
            _imageSpinner.visibility = View.GONE
        }

        if (message != null) {
            _textMessage.visibility = View.VISIBLE
            _textMessage.text = message
        } else {
            _textMessage.visibility = View.GONE
        }

        _layoutOverlay.visibility = if (isLoading || message != null) View.VISIBLE else View.GONE
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
        _playerControlView.player = null
        NetworkService.activityCount--

        GlobalScope.launch(Dispatchers.IO) {
            try {
                NetworkService.instance?.sendPlaybackUpdate(PlaybackUpdateMessage(
                    System.currentTimeMillis(),
                    0.0,
                    0.0,
                    0,
                    0.0
                ))
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to send playback update.", e)
            }
        }
    }

    @OptIn(UnstableApi::class)
    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (_playerControlView.isControllerFullyVisible) {
            if (event.keyCode == KeyEvent.KEYCODE_BACK) {
                _playerControlView.hideController()
                return true
            }
        } else {
            when (event.keyCode) {
                KeyEvent.KEYCODE_DPAD_LEFT -> {
                    _exoPlayer.seekTo(max(0, _exoPlayer.currentPosition - SEEK_BACKWARD_MILLIS))
                    return true
                }
                KeyEvent.KEYCODE_DPAD_RIGHT -> {
                    _exoPlayer.seekTo(_exoPlayer.currentPosition + SEEK_FORWARD_MILLIS)
                    return true
                }
            }
        }

        return super.dispatchKeyEvent(event)
    }

    @OptIn(UnstableApi::class)
    fun play(playMessage: PlayMessage) {
        val mediaItemBuilder = MediaItem.Builder()
        if (playMessage.container.isNotEmpty()) {
            mediaItemBuilder.setMimeType(playMessage.container)
        }

        if (!playMessage.url.isNullOrEmpty()) {
            mediaItemBuilder.setUri(Uri.parse(playMessage.url))
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

        val mediaItem = mediaItemBuilder.build()
        val mediaSource = when (playMessage.container) {
            "application/dash+xml" -> DashMediaSource.Factory(dataSourceFactory).createMediaSource(mediaItem)
            "application/x-mpegurl" -> HlsMediaSource.Factory(dataSourceFactory).createMediaSource(mediaItem)
            "application/vnd.apple.mpegurl" -> HlsMediaSource.Factory(dataSourceFactory).createMediaSource(mediaItem)
            else -> DefaultMediaSourceFactory(dataSourceFactory).createMediaSource(mediaItem)
        }

        _exoPlayer.setMediaSource(mediaSource)
        _exoPlayer.setPlaybackSpeed(playMessage.speed?.toFloat() ?: 1.0f)

        if (playMessage.time != null) {
            _exoPlayer.seekTo((playMessage.time * 1000).toLong())
        }

        setStatus(true, null)
        _wasPlaying = false
        _exoPlayer.playWhenReady = true
        _exoPlayer.prepare()
        _exoPlayer.play()
    }

    fun pause() {
        _exoPlayer.pause()
    }

    fun resume() {
        _exoPlayer.play()
    }

    fun seek(seekMessage: SeekMessage) {
        _exoPlayer.seekTo((seekMessage.time * 1000.0).toLong())
    }

    fun setSpeed(setSpeedMessage: SetSpeedMessage) {
        _exoPlayer.setPlaybackSpeed(setSpeedMessage.speed.toFloat())
    }

    fun setVolume(setVolumeMessage: SetVolumeMessage) {
        _exoPlayer.volume = setVolumeMessage.volume.toFloat()
    }

    companion object {
        var instance: PlayerActivity? = null
        private const val TAG = "PlayerActivity"

        private const val SEEK_BACKWARD_MILLIS = 10_000
        private const val SEEK_FORWARD_MILLIS = 10_000
    }
}
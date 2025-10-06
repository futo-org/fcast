package com.futo.fcast.receiver

import android.os.Handler
import android.os.Looper
import android.util.Log
import com.futo.fcast.receiver.models.MediaItem
import com.futo.fcast.receiver.models.PlayMessage
import okhttp3.OkHttpClient
import java.util.Calendar

const val TAG = "Utility"

fun ensureNotMainThread() {
    if (Looper.myLooper() == Looper.getMainLooper()) {
        Log.e(
            TAG,
            "Throwing exception because a function that should not be called on main thread, is called on main thread"
        )
        throw IllegalStateException("Cannot run on main thread")
    }
}

inline fun setTimeout(crossinline block: () -> Unit, timeoutMillis: Long) {
    setTimeout(Handler(Looper.getMainLooper()), block, timeoutMillis)
}

inline fun setTimeout(
    handler: Handler,
    crossinline block: () -> Unit,
    timeoutMillis: Long
): Runnable {
    val runnable = Runnable { block() }
    handler.postDelayed(runnable, timeoutMillis)
    return runnable
}

inline fun setInterval(crossinline block: () -> Unit, interval: Long) {
    setInterval(Handler(Looper.getMainLooper()), block, interval)
}

inline fun setInterval(handler: Handler, crossinline block: () -> Unit, interval: Long): Runnable {
    val runnable = object : Runnable {
        override fun run() {
            block()
            handler.postDelayed(this, interval)
        }
    }
    handler.post(runnable)
    return runnable
}


// preparePlayMessage defined in NetworkService.kt

fun fetchJSON(url: String): String {
    val client = OkHttpClient()
    val request = okhttp3.Request.Builder()
        .method("GET", null)
        .url(url)
        .build()

    val response = client.newCall(request).execute()
    if (!response.isSuccessful) {
        throw Exception("Error fetching JSON: $response")
    }

    return response.body.string()
}

fun playMessageFromMediaItem(item: MediaItem?): PlayMessage {
    return if (item != null) PlayMessage(
        item.container, item.url,
        item.content, item.time, item.volume, item.speed,
        item.headers, item.metadata
    ) else PlayMessage("")
}

fun mediaItemFromPlayMessage(message: PlayMessage?): MediaItem {
    return if (message != null) MediaItem(
        message.container, message.url,
        message.content, message.time, message.volume, message.speed,
        null, null, message.headers, message.metadata
    )
    else MediaItem("")
}

class Timer(
    private var _callback: () -> Unit,
    private var _delay: Long,
    autoStart: Boolean = true
) {
    private val _handler = Handler(Looper.getMainLooper())
    private var _handle: Runnable?
    private var _startTime: Long = Calendar.getInstance().time.time
    private var _remainingTime: Long? = null
    private var _enabled: Boolean = true

    var started: Boolean = false

    init {
        _handle = null

        if (autoStart) {
            start()
        }
    }

    fun start(delay: Long? = null) {
        if (_enabled) {
            _delay = delay ?: _delay

            _handle?.let {
                _handler.removeCallbacks(it)
            }

            started = true
            _startTime = Calendar.getInstance().time.time
            _remainingTime = null
            _handle = setTimeout(_handler, _callback, _delay)
        }
    }

    fun pause() {
        if (_enabled && _handle != null) {
            _handler.removeCallbacks(_handle!!)
            _handle = null
            _remainingTime = _delay - (Calendar.getInstance().time.time - _startTime)
        }
    }

    fun resume() {
        if (_enabled && _remainingTime != null) {
            start(_remainingTime)
        }
    }

    fun stop() {
        _handle?.let {
            _handler.removeCallbacks(it)
            _handle = null
            _remainingTime = null
            started = false
        }
    }

    fun end() {
        stop()
        _callback()
    }

    fun enable() {
        _enabled = true
    }

    fun disable() {
        _enabled = false
        stop()
    }

    fun setDelay(delay: Long) {
        stop()
        _delay = delay
    }

    fun setCallback(callback: () -> Unit) {
        stop()
        _callback = callback
    }

    fun isPaused(): Boolean {
        return _remainingTime != null
    }

    fun restart() {
        stop()
        start()
    }
}

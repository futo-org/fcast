package com.futo.fcast.receiver

import android.os.Looper
import android.util.Log

fun ensureNotMainThread() {
    if (Looper.myLooper() == Looper.getMainLooper()) {
        Log.e("Utility", "Throwing exception because a function that should not be called on main thread, is called on main thread")
        throw IllegalStateException("Cannot run on main thread")
    }
}
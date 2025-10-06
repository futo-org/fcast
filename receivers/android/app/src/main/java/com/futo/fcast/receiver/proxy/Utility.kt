package com.futo.fcast.receiver.proxy

import android.util.Log
import kotlinx.serialization.json.Json
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.io.InputStream

// Wrapper class to avoid modifying server implementation
class Logger {
    companion object {
        fun i(tag: String?, msg: String) {
            Log.i(tag, msg)
        }
        fun w(tag: String?, msg: String) {
            Log.w(tag, msg)
        }
        fun e(tag: String?, msg: String) {
            Log.e(tag, msg)
        }
        fun e(tag: String?, msg: String, tr: Throwable?) {
            Log.e(tag, msg, tr)
        }
        fun v(tag: String?, msg: String) {
            Log.v(tag, msg)
        }
    }
}

class Serializer {
    companion object {
        val json = Json { ignoreUnknownKeys = true; encodeDefaults = true; coerceInputValues = true };
    }
}

fun InputStream.readHttpHeaderBytes() : ByteArray {
    val headerBytes = ByteArrayOutputStream()
    var crlfCount = 0

    while (crlfCount < 4) {
        val b = read()
        if (b == -1) {
            throw IOException("Unexpected end of stream while reading headers")
        }

        if (b == 0x0D || b == 0x0A) { // CR or LF
            crlfCount++
        } else {
            crlfCount = 0
        }

        headerBytes.write(b)
    }

    return headerBytes.toByteArray()
}

fun InputStream.readLine() : String? {
    val line = ByteArrayOutputStream()
    var crlfCount = 0

    while (crlfCount < 2) {
        val b = read()
        if (b == -1) {
            return null
        }

        if (b == 0x0D || b == 0x0A) { // CR or LF
            crlfCount++
        } else {
            crlfCount = 0
            line.write(b)
        }
    }

    return String(line.toByteArray(), Charsets.UTF_8)
}

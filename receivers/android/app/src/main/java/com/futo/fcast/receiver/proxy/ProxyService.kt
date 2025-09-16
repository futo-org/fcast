package com.futo.fcast.receiver.proxy

import android.util.Log
import com.futo.fcast.receiver.NetworkService
import com.futo.fcast.receiver.models.PlayMessage
import com.futo.fcast.receiver.models.streamingMediaTypes
import com.futo.fcast.receiver.proxy.server.ManagedHttpServer
import com.futo.fcast.receiver.proxy.server.handlers.HttpProxyHandler
import java.util.UUID

class ProxyService {
    private var _stopped: Boolean = true

    fun start() {
        Log.i(TAG, "Starting ProxyService")
        if (!_stopped) {
            return
        }
        _stopped = false

        proxyServer = ManagedHttpServer()
        proxyServerAddress = proxyServer?.getAddress()
        proxyServer?.start()
        Log.i(TAG, "Started ProxyService")
    }

    fun stop() {
        Log.i(TAG, "Stopping ProxyService")
        if (_stopped) {
            return
        }
        _stopped = true

        proxyServer?.stop()
        Log.i(TAG, "Stopped ProxyService")
    }

    companion object {
        private const val TAG = "ProxyService"

        var proxyServer: ManagedHttpServer? = null
        var proxyServerAddress: String? = null
        var proxiedFiles: MutableMap<String, PlayMessage> = mutableMapOf()

        fun proxyPlayIfRequired(message: PlayMessage): PlayMessage {
            if (message.url != null && (message.url.startsWith("app://") || (message.headers != null && !streamingMediaTypes.contains(
                    message.container.lowercase()
                )))
            ) {
                return PlayMessage(
                    message.container, proxyFile(message),
                    message.content, message.time, message.volume,
                    message.speed, message.headers
                )
            }
            return message
        }

        fun proxyFile(message: PlayMessage): String {
            val path = UUID.randomUUID()
            val proxiedUrl = "http://${proxyServerAddress}:${proxyServer?.port}/$path"
            Log.i(NetworkService.Companion.TAG, "Proxied url $proxiedUrl, $message")
            proxiedFiles[proxiedUrl] = message

            if (message.url!!.startsWith("app://")) {
//                var start: number = 0
//                var end: number = null
//                val contentSize = MediaCache.getInstance().getObjectSize(proxyInfo.url)
//                if (req.headers.range) {
//                    val range = req.headers.range.slice(6).split('-')
//                    start = (range.length > 0) ? parseInt(range[0]) : 0
//                    end = (range.length > 1) ? parseInt(range[1]) : null
//                }
//
//                Log.d(TAG, "Fetching byte range from cache: start=${start}, end=${end}")
//                val stream = MediaCache.getInstance().getObject(proxyInfo.url, start, end)
//                var responseCode = null
//                var responseHeaders = null
//
//                if (start != 0) {
//                    responseCode = 206
//                    responseHeaders = {
//                        'Accept-Ranges': 'bytes',
//                        'Content-Length': contentSize - start,
//                        'Content-Range': `bytes ${start}-${end ? end : contentSize - 1}/${contentSize}`,
//                        'Content-Type': proxyInfo.container,
//                    }
//                }
//                else {
//                    responseCode = 200
//                    responseHeaders = {
//                        'Accept-Ranges': 'bytes',
//                        'Content-Length': contentSize,
//                        'Content-Type': proxyInfo.container,
//                    }
//                }
//
//                Log.d(TAG,"Serving content ${proxyInfo.url} with response headers: $responseHeaders")
//                res.writeHead(responseCode, responseHeaders)
//                stream.pipe(res)
            } else {
                val handler = HttpProxyHandler("GET", "/$path", message.url, true)
                    .withInjectedHost()
                    .withHeader("Access-Control-Allow-Origin", "*")

                message.headers?.forEach {
                    handler.withHeader(it.key, it.value)
                }

                proxyServer?.addHandlerWithAllowAllOptions(handler, true)?.withTag("cast")
            }

            return proxiedUrl
        }
    }
}

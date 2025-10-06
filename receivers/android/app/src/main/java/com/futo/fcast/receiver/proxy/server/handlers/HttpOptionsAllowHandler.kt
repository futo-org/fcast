package com.futo.fcast.receiver.proxy.server.handlers

import com.futo.fcast.receiver.proxy.server.HttpContext

class HttpOptionsAllowHandler(path: String, val allowedMethods: List<String> = listOf()) : HttpHandler("OPTIONS", path) {
    override fun handle(httpContext: HttpContext) {
        val newHeaders = headers.clone()
        newHeaders.put("Access-Control-Allow-Origin", "*")

        if (allowedMethods.isNotEmpty()) {
            newHeaders.put("Access-Control-Allow-Methods", allowedMethods.map { it.uppercase() }.joinToString(", "))
        } else {
            newHeaders.put("Access-Control-Allow-Methods", "*")
        }

        newHeaders.put("Access-Control-Allow-Headers", "*")
        httpContext.respondCode(200, newHeaders);
    }
}
package com.futo.fcast.receiver.proxy.server.handlers

import com.futo.fcast.receiver.proxy.server.HttpContext

class HttpFunctionHandler(method: String, path: String, val handler: (HttpContext)->Unit) : HttpHandler(method, path) {
    override fun handle(httpContext: HttpContext) {
        httpContext.setResponseHeaders(this.headers);
        handler(httpContext);
    }
}
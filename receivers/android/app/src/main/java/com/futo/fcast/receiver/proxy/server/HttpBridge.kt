package com.futo.fcast.receiver.proxy.server

@Target(AnnotationTarget.FIELD, AnnotationTarget.FUNCTION)
@Retention(AnnotationRetention.RUNTIME)
annotation class HttpGET(val path: String, val contentType: String = "");

@Target(AnnotationTarget.FIELD, AnnotationTarget.FUNCTION)
@Retention(AnnotationRetention.RUNTIME)
annotation class HttpPOST(val path: String, val contentType: String = "");

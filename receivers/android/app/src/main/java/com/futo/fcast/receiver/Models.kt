package com.futo.fcast.receiver

import kotlinx.serialization.Serializable

@Serializable
data class FCastNetworkConfig(
    val ips: List<String>,
    val services: List<FCastService>
)

@Serializable
data class FCastService(
    val port: Int,
    val type: Int
)
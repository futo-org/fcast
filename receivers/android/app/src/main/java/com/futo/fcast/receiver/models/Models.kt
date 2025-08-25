package com.futo.fcast.receiver.models

import kotlinx.serialization.Serializable

@Serializable
data class FCastNetworkConfig(
    val name: String,
    val addresses: List<String>,
    val services: List<FCastService>
)

@Serializable
data class FCastService(
    val port: Int,
    val type: Int
)
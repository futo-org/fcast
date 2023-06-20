package com.futo.fcast.receiver

import kotlinx.serialization.Serializable

@Serializable
data class PlayMessage(
    val container: String,
    val url: String? = null,
    val content: String? = null,
    val time: Long? = null
)

@Serializable
data class SeekMessage(
    val time: Long
)

@Serializable
data class PlaybackUpdateMessage(
    val time: Long,
    val state: Int
)

@Serializable
data class VolumeUpdateMessage(
    val volume: Double
)

@Serializable
data class SetVolumeMessage(
    val volume: Double
)
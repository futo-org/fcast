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
    val time: Double
)

@Serializable
data class PlaybackUpdateMessage(
    val generationTime: Long,
    val time: Double,
    val duration: Double,
    val state: Int
)

@Serializable
data class VolumeUpdateMessage(
    val generationTime: Long,
    val volume: Double
)

@Serializable
data class SetVolumeMessage(
    val volume: Double
)
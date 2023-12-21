package com.futo.fcast.receiver

import kotlinx.serialization.Serializable

@Serializable
data class PlayMessage(
    val container: String,
    val url: String? = null,
    val content: String? = null,
    val time: Double? = null,
    val speed: Double? = null
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
    val state: Int,
    val speed: Double
)

@Serializable
data class VolumeUpdateMessage(
    val generationTime: Long,
    val volume: Double
)

@Serializable
data class PlaybackErrorMessage(
    val message: String
)

@Serializable
data class SetSpeedMessage(
    val speed: Double
)

@Serializable
data class SetVolumeMessage(
    val volume: Double
)

@Serializable
data class VersionMessage(
    val version: Long
)

@Serializable
data class KeyExchangeMessage(
    val version: Long,
    val publicKey: String
)

@Serializable
data class DecryptedMessage(
    val opcode: Long,
    val message: String?
)

@Serializable
data class EncryptedMessage(
    val version: Long,
    val iv: String?,
    val blob: String
)
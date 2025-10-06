package com.futo.fcast.receiver.models

import kotlinx.serialization.Serializable

@Serializable
data class PlayMessageV1(
    val container: String,            // The MIME type (video/mp4)
    val url: String? = null,          // The URL to load (optional)
    val content: String? = null,      // The content to load (i.e. a DASH manifest, json content, optional)
    val time: Double? = null,         // The time to start playing in seconds
)

@Serializable
data class PlayMessageV2(
    val container: String,            // The MIME type (video/mp4)
    val url: String? = null,          // The URL to load (optional)
    val content: String? = null,      // The content to load (i.e. a DASH manifest, json content, optional)
    val time: Double? = null,         // The time to start playing in seconds
    val speed: Double? = null,        // The factor to multiply playback speed by (defaults to 1.0)
    val headers: Map<String, String>? = null,  // HTTP request headers to add to the play request Map<string, string>
)

@Serializable
data class PlaybackUpdateMessageV1(
    val state: Int,                   // The playback state
    val time: Double,                 // The current time playing in seconds
)

@Serializable
data class PlaybackUpdateMessageV2(
    val generationTime: Long,         // The time the packet was generated (unix time milliseconds)
    val state: Int,                   // The playback state
    val time: Double,                 // The current time playing in seconds
    val duration: Double,             // The duration in seconds
    val speed: Double,                // The playback speed factor
)

@Serializable
data class VolumeUpdateMessageV1(
    val volume: Double,               // The current volume (0-1)
)

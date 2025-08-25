package com.futo.fcast.receiver.models

import kotlinx.serialization.Contextual
import kotlinx.serialization.Serializable

// Protocol Documentation: https://gitlab.futo.org/videostreaming/fcast/-/wikis/Protocol-version-3
const val PROTOCOL_VERSION = 3L

enum class Opcode(val value: Byte) {
    None(0),
    Play(1),
    Pause(2),
    Resume(3),
    Stop(4),
    Seek(5),
    PlaybackUpdate(6),
    VolumeUpdate(7),
    SetVolume(8),
    PlaybackError(9),
    SetSpeed(10),
    Version(11),
    Ping(12),
    Pong(13),
    Initial(14),
    PlayUpdate(15),
    SetPlaylistItem(16),
    SubscribeEvent(17),
    UnsubscribeEvent(18),
    Event(19);

    companion object {
        private val _map = entries.associateBy { it.value }
        fun find(value: Byte): Opcode = _map[value] ?: None
    }
}

enum class PlaybackState(val value: Byte) {
    Idle(0),
    Playing(1),
    Paused(2),
}

enum class ContentType(val value: Byte) {
    Playlist(0),
}

enum class MetadataType(val value: Byte) {
    Generic(0),
}

enum class EventType(val value: Byte) {
    MediaItemStart(0),
    MediaItemEnd(1),
    MediaItemChange(2),
    KeyDown(3),
    KeyUp(4),
}

// Required supported keys for listener events defined below.
// Optionally supported key values list: https://developer.mozilla.org/en-US/docs/Web/API/UI_Events/Keyboard_event_key_values
enum class KeyNames(val value: String) {
    Left("ArrowLeft"),
    Right("ArrowRight"),
    Up("ArrowUp"),
    Down("ArrowDown"),
    Ok("Enter"),
}

interface MetadataObject {
    val type: MetadataType
}

@Serializable
data class GenericMediaMetadata(
    override val type: MetadataType = MetadataType.Generic,

    val title: String? = null,
    val thumbnailUrl: String? = null,
    @Contextual
    val custom: Any? = null,
) : MetadataObject

@Serializable
data class PlayMessage(
    val container: String,            // The MIME type (video/mp4)
    val url: String? = null,          // The URL to load (optional)
    val content: String? = null,      // The content to load (i.e. a DASH manifest, json content, optional)
    val time: Double? = null,         // The time to start playing in seconds
    val volume: Double? = null,       // The desired volume (0-1)
    val speed: Double? = null,        // The factor to multiply playback speed by (defaults to 1.0)
    val headers: Map<String, String>? = null,  // HTTP request headers to add to the play request Map<string, string>
    val metadata: MetadataObject? = null,
)

@Serializable
data class SeekMessage(
    val time: Double,                 // The time to seek to in seconds
)

@Serializable
data class PlaybackUpdateMessage(
    val generationTime: Long,         // The time the packet was generated (unix time milliseconds)
    val state: Int,                   // The playback state
    val time: Double? = null,         // The current time playing in seconds
    val duration: Double? = null,     // The duration in seconds
    val speed: Double? = null,        // The playback speed factor
    val itemIndex: Int? = null,       // The playlist item index currently being played on receiver
)

@Serializable
data class VolumeUpdateMessage(
    val generationTime: Long,         // The time the packet was generated (unix time milliseconds)
    val volume: Double,               // The current volume (0-1)
)

@Serializable
data class SetVolumeMessage(
    val volume: Double,               // The desired volume (0-1)
)

@Serializable
data class PlaybackErrorMessage(
    val message: String
)

@Serializable
data class SetSpeedMessage(
    val speed: Double,                // The factor to multiply playback speed by
)

@Serializable
data class VersionMessage(
    val version: Long,                // Protocol version number (integer)
)

interface ContentObject {
    val contentType: ContentType
}

@Serializable
data class MediaItem(
    val container: String,            // The MIME type (video/mp4)
    val url: String? = null,          // The URL to load (optional)
    val content: String? = null,      // The content to load (i.e. a DASH manifest, json content, optional)
    val time: Double? = null,         // The time to start playing in seconds
    val volume: Double? = null,       // The desired volume (0-1)
    val speed: Double? = null,        // The factor to multiply playback speed by (defaults to 1.0)
    val cache: Boolean? = null,       // Indicates if the receiver should preload the media item
    val showDuration: Double? = null, // Indicates how long the item content is presented on screen in seconds
    val headers: Map<String, String>? = null,  // HTTP request headers to add to the play request Map<string, string>
    val metadata: MetadataObject? = null,
)

@Serializable
data class PlaylistContent(
    override val contentType: ContentType = ContentType.Playlist,

    val items: ArrayList<MediaItem>,
    val offset: Int? = null,         // Start position of the first item to play from the playlist
    val volume: Double? = null,      // The desired volume (0-1)
    val speed: Double? = null,       // The factor to multiply playback speed by (defaults to 1.0)
    val forwardCache: Int? = null,   // Count of media items should be pre-loaded forward from the current view index
    val backwardCache: Int? = null,  // Count of media items should be pre-loaded backward from the current view index
    val metadata: MetadataObject? = null,
) : ContentObject

@Serializable
data class InitialSenderMessage(
    val displayName: String? = null,
    val appName: String? = null,
    val appVersion: String? = null,
)

@Serializable
data class InitialReceiverMessage(
    val displayName: String? = null,
    val appName: String? = null,
    val appVersion: String? = null,
    val playData: PlayMessage? = null,
)

@Serializable
data class PlayUpdateMessage(
    val generationTime: Long,
    val playData: PlayMessage? = null,
)

@Serializable
data class SetPlaylistItemMessage(
    val itemIndex: Int,          // The playlist item index to play on receiver
)

interface EventSubscribeObject {
    val type: EventType
}

interface EventObject {
    val type: EventType
}

@Serializable
data class MediaItemStartEvent(
    override val type: EventType = EventType.MediaItemStart
) : EventSubscribeObject

@Serializable
data class MediaItemEndEvent(
    override val type: EventType = EventType.MediaItemEnd
) : EventSubscribeObject

@Serializable
data class MediaItemChangeEvent(
    override val type: EventType = EventType.MediaItemChange
) : EventSubscribeObject

@Serializable
data class KeyDownEvent(
    override val type: EventType = EventType.KeyDown,

    val keys: ArrayList<String>,
) : EventSubscribeObject

@Serializable
data class KeyUpEvent(
    override val type: EventType = EventType.KeyUp,

    val keys: ArrayList<String>,
) : EventSubscribeObject

@Serializable
data class SubscribeEventMessage(
    val event: EventSubscribeObject,
)

@Serializable
data class UnsubscribeEventMessage(
    val event: EventSubscribeObject,
)

@Serializable
data class MediaItemEvent(
    override val type: EventType,
    val mediaItem: MediaItem,
) : EventObject

@Serializable
data class KeyEvent(
    override val type: EventType,
    val key: String,
    val repeat: Boolean,
    val handled: Boolean,
) : EventObject

@Serializable
data class EventMessage(
    val generationTime: Long,
    val event: EventObject,
)

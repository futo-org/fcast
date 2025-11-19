package com.futo.fcast.receiver.models

import kotlinx.serialization.Contextual
import kotlinx.serialization.ExperimentalSerializationApi
import kotlinx.serialization.InternalSerializationApi
import kotlinx.serialization.KSerializer
import kotlinx.serialization.Serializable
import kotlinx.serialization.descriptors.PolymorphicKind
import kotlinx.serialization.descriptors.PrimitiveKind
import kotlinx.serialization.descriptors.PrimitiveSerialDescriptor
import kotlinx.serialization.descriptors.SerialDescriptor
import kotlinx.serialization.descriptors.buildSerialDescriptor
import kotlinx.serialization.encoding.Decoder
import kotlinx.serialization.encoding.Encoder
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonDecoder
import kotlinx.serialization.json.JsonEncoder
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.int
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive

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

@Serializable(with = ContentTypeSerializer::class)
enum class ContentType(val value: Byte) {
    Playlist(0),
}

@Serializable(with = MetadataTypeSerializer::class)
enum class MetadataType(val value: Byte) {
    Generic(0),
}

@Serializable(with = EventTypeSerializer::class)
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

@Serializable(with = MetadataObjectSerializer::class)
sealed interface MetadataObject {
    val type: MetadataType
}

@Serializable
data class GenericMediaMetadata(
    @Serializable(with = MetadataTypeSerializer::class)
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
    @Serializable(with = MetadataObjectSerializer::class)
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

@Serializable(with = ContentObjectSerializer::class)
sealed interface ContentObject {
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
    @Serializable(with = MetadataObjectSerializer::class)
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
    @Serializable(with = MetadataObjectSerializer::class)
    val metadata: MetadataObject? = null,
) : ContentObject

@Serializable
data class InitialSenderMessage(
    val displayName: String? = null,
    val appName: String? = null,
    val appVersion: String? = null,
)

@Serializable
data class LivestreamCapabilities(
    val whep: Boolean? = null,
)

@Serializable
data class AVCapabilities(
    val livestream: LivestreamCapabilities? = null,
)

@Serializable
data class ReceiverCapabilities(
    val av: AVCapabilities? = null,
)

@Serializable
data class InitialReceiverMessage(
    val displayName: String? = null,
    val appName: String? = null,
    val appVersion: String? = null,
    val playData: PlayMessage? = null,
    val experimentalCapabilities: ReceiverCapabilities? = null,
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

@Serializable(with = EventSubscribeObjectSerializer::class)
sealed interface EventSubscribeObject {
    val type: EventType
}

@Serializable(with = EventObjectSerializer::class)
sealed interface EventObject {
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
    @Serializable(with = EventSubscribeObjectSerializer::class)
    val event: EventSubscribeObject,
)

@Serializable
data class UnsubscribeEventMessage(
    @Serializable(with = EventSubscribeObjectSerializer::class)
    val event: EventSubscribeObject,
)

@Serializable
data class MediaItemEvent(
    override val type: EventType,
    val item: MediaItem,
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
    @Serializable(with = EventObjectSerializer::class)
    val event: EventObject,
)


object ContentTypeSerializer : KSerializer<ContentType> {
    override val descriptor: SerialDescriptor =
        PrimitiveSerialDescriptor("ContentType", PrimitiveKind.BYTE)

    override fun serialize(encoder: Encoder, value: ContentType) {
        encoder.encodeByte(value.value)
    }

    override fun deserialize(decoder: Decoder): ContentType {
        val byteValue = decoder.decodeByte()
        return ContentType.entries.first { it.value == byteValue }
    }
}

object MetadataTypeSerializer : KSerializer<MetadataType> {
    override val descriptor: SerialDescriptor =
        PrimitiveSerialDescriptor("MetadataType", PrimitiveKind.BYTE)

    override fun serialize(encoder: Encoder, value: MetadataType) {
        encoder.encodeByte(value.value)
    }

    override fun deserialize(decoder: Decoder): MetadataType {
        val byteValue = decoder.decodeByte()
        return MetadataType.entries.first { it.value == byteValue }
    }
}

object EventTypeSerializer : KSerializer<EventType> {
    override val descriptor: SerialDescriptor =
        PrimitiveSerialDescriptor("EventType", PrimitiveKind.BYTE)

    override fun serialize(encoder: Encoder, value: EventType) {
        encoder.encodeByte(value.value)
    }

    override fun deserialize(decoder: Decoder): EventType {
        val byteValue = decoder.decodeByte()
        return EventType.entries.first { it.value == byteValue }
    }
}

object MetadataObjectSerializer : KSerializer<MetadataObject> {
    @OptIn(InternalSerializationApi::class, ExperimentalSerializationApi::class)
    override val descriptor: SerialDescriptor =
        buildSerialDescriptor("MetadataObject", PolymorphicKind.SEALED)

    override fun deserialize(decoder: Decoder): MetadataObject {
        val jsonDecoder = decoder as? JsonDecoder ?: error("This serializer works only with Json")
        val jsonObject = jsonDecoder.decodeJsonElement().jsonObject
        val typeValue = jsonObject["type"]?.jsonPrimitive?.int ?: error("Missing 'type' field")

        return when (typeValue) {
            MetadataType.Generic.value.toInt() -> jsonDecoder.json.decodeFromJsonElement(
                GenericMediaMetadata.serializer(),
                jsonObject
            )

            else -> error("Unknown metadata type: $typeValue")
        }
    }

    override fun serialize(encoder: Encoder, value: MetadataObject) {
        val json = Json { ignoreUnknownKeys = true }
        val jsonEncoder = encoder as? JsonEncoder ?: error("This serializer works only with Json")

        when (value) {
            is GenericMediaMetadata -> {
                val jsonObject =
                    json.encodeToJsonElement(GenericMediaMetadata.serializer(), value).jsonObject
                val map = jsonObject.toMutableMap()
                map["type"] = JsonPrimitive(value.type.value)
                jsonEncoder.encodeJsonElement(JsonObject(map))
            }
        }
    }
}

object ContentObjectSerializer : KSerializer<ContentObject> {
    @OptIn(InternalSerializationApi::class, ExperimentalSerializationApi::class)
    override val descriptor: SerialDescriptor =
        buildSerialDescriptor("ContentObject", PolymorphicKind.SEALED)

    override fun deserialize(decoder: Decoder): ContentObject {
        val jsonDecoder = decoder as? JsonDecoder ?: error("This serializer works only with Json")
        val jsonObject = jsonDecoder.decodeJsonElement().jsonObject
        val typeValue =
            jsonObject["contentType"]?.jsonPrimitive?.int ?: error("Missing 'type' field")

        return when (typeValue) {
            ContentType.Playlist.value.toInt() -> jsonDecoder.json.decodeFromJsonElement(
                PlaylistContent.serializer(),
                jsonObject
            )

            else -> error("Unknown metadata type: $typeValue")
        }
    }

    override fun serialize(encoder: Encoder, value: ContentObject) {
        val jsonEncoder = encoder as? JsonEncoder ?: error("This serializer works only with Json")

        when (value) {
            is PlaylistContent -> jsonEncoder.encodeSerializableValue(
                PlaylistContent.serializer(),
                value
            )
        }
    }
}

object EventSubscribeObjectSerializer : KSerializer<EventSubscribeObject> {
    @OptIn(InternalSerializationApi::class, ExperimentalSerializationApi::class)
    override val descriptor: SerialDescriptor =
        buildSerialDescriptor("EventSubscribeObject", PolymorphicKind.SEALED)

    override fun deserialize(decoder: Decoder): EventSubscribeObject {
        val jsonDecoder = decoder as? JsonDecoder ?: error("This serializer works only with Json")
        val jsonObject = jsonDecoder.decodeJsonElement().jsonObject
        val typeValue = jsonObject["type"]?.jsonPrimitive?.int ?: error("Missing 'type' field")

        return when (typeValue) {
            EventType.MediaItemStart.value.toInt() -> jsonDecoder.json.decodeFromJsonElement(
                MediaItemStartEvent.serializer(),
                jsonObject
            )

            EventType.MediaItemEnd.value.toInt() -> jsonDecoder.json.decodeFromJsonElement(
                MediaItemEndEvent.serializer(),
                jsonObject
            )

            EventType.MediaItemChange.value.toInt() -> jsonDecoder.json.decodeFromJsonElement(
                MediaItemChangeEvent.serializer(),
                jsonObject
            )

            EventType.KeyDown.value.toInt() -> jsonDecoder.json.decodeFromJsonElement(
                KeyDownEvent.serializer(),
                jsonObject
            )

            EventType.KeyUp.value.toInt() -> jsonDecoder.json.decodeFromJsonElement(
                KeyUpEvent.serializer(),
                jsonObject
            )

            else -> error("Unknown metadata type: $typeValue")
        }
    }

    override fun serialize(encoder: Encoder, value: EventSubscribeObject) {
        val jsonEncoder = encoder as? JsonEncoder ?: error("This serializer works only with Json")

        when (value) {
            is MediaItemStartEvent -> jsonEncoder.encodeSerializableValue(
                MediaItemStartEvent.serializer(),
                value
            )

            is MediaItemEndEvent -> jsonEncoder.encodeSerializableValue(
                MediaItemEndEvent.serializer(),
                value
            )

            is MediaItemChangeEvent -> jsonEncoder.encodeSerializableValue(
                MediaItemChangeEvent.serializer(),
                value
            )

            is KeyDownEvent -> jsonEncoder.encodeSerializableValue(KeyDownEvent.serializer(), value)
            is KeyUpEvent -> jsonEncoder.encodeSerializableValue(KeyUpEvent.serializer(), value)
        }
    }
}

object EventObjectSerializer : KSerializer<EventObject> {
    @OptIn(InternalSerializationApi::class, ExperimentalSerializationApi::class)
    override val descriptor: SerialDescriptor =
        buildSerialDescriptor("EventObject", PolymorphicKind.SEALED)

    override fun deserialize(decoder: Decoder): EventObject {
        val jsonDecoder = decoder as? JsonDecoder ?: error("This serializer works only with Json")
        val jsonObject = jsonDecoder.decodeJsonElement().jsonObject
        val typeValue = jsonObject["type"]?.jsonPrimitive?.int ?: error("Missing 'type' field")

        return when (typeValue) {
            EventType.MediaItemStart.value.toInt() -> jsonDecoder.json.decodeFromJsonElement(
                MediaItemEvent.serializer(),
                jsonObject
            )

            EventType.MediaItemEnd.value.toInt() -> jsonDecoder.json.decodeFromJsonElement(
                MediaItemEvent.serializer(),
                jsonObject
            )

            EventType.MediaItemChange.value.toInt() -> jsonDecoder.json.decodeFromJsonElement(
                MediaItemEvent.serializer(),
                jsonObject
            )

            EventType.KeyDown.value.toInt() -> jsonDecoder.json.decodeFromJsonElement(
                KeyEvent.serializer(),
                jsonObject
            )

            EventType.KeyUp.value.toInt() -> jsonDecoder.json.decodeFromJsonElement(
                KeyEvent.serializer(),
                jsonObject
            )

            else -> error("Unknown metadata type: $typeValue")
        }
    }

    override fun serialize(encoder: Encoder, value: EventObject) {
        val jsonEncoder = encoder as? JsonEncoder ?: error("This serializer works only with Json")

        when (value) {
            is MediaItemEvent -> jsonEncoder.encodeSerializableValue(
                MediaItemEvent.serializer(),
                value
            )

            is KeyEvent -> jsonEncoder.encodeSerializableValue(KeyEvent.serializer(), value)
        }
    }
}

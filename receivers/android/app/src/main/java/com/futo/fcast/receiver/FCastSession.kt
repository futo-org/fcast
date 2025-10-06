package com.futo.fcast.receiver

import android.util.Log
import com.futo.fcast.receiver.models.EventMessage
import com.futo.fcast.receiver.models.InitialReceiverMessage
import com.futo.fcast.receiver.models.InitialSenderMessage
import com.futo.fcast.receiver.models.Opcode
import com.futo.fcast.receiver.models.PROTOCOL_VERSION
import com.futo.fcast.receiver.models.PlayMessage
import com.futo.fcast.receiver.models.PlayMessageV1
import com.futo.fcast.receiver.models.PlayMessageV2
import com.futo.fcast.receiver.models.PlayUpdateMessage
import com.futo.fcast.receiver.models.PlaybackErrorMessage
import com.futo.fcast.receiver.models.PlaybackUpdateMessage
import com.futo.fcast.receiver.models.PlaybackUpdateMessageV1
import com.futo.fcast.receiver.models.PlaybackUpdateMessageV2
import com.futo.fcast.receiver.models.SeekMessage
import com.futo.fcast.receiver.models.SetPlaylistItemMessage
import com.futo.fcast.receiver.models.SetSpeedMessage
import com.futo.fcast.receiver.models.SetVolumeMessage
import com.futo.fcast.receiver.models.SubscribeEventMessage
import com.futo.fcast.receiver.models.UnsubscribeEventMessage
import com.futo.fcast.receiver.models.VersionMessage
import com.futo.fcast.receiver.models.VolumeUpdateMessage
import com.futo.fcast.receiver.models.VolumeUpdateMessageV1
import kotlinx.serialization.json.Json
import java.io.DataOutputStream
import java.io.OutputStream
import java.net.SocketAddress
import java.util.UUID

enum class SessionState {
    Idle,
    WaitingForLength,
    WaitingForData,
    Disconnected
}

const val LENGTH_BYTES = 4
const val MAXIMUM_PACKET_LENGTH = 32000

class FCastSession(
    outputStream: OutputStream,
    private val _remoteSocketAddress: SocketAddress,
    private val _service: NetworkService
) {
    private var _buffer = ByteArray(MAXIMUM_PACKET_LENGTH)
    private var _bytesRead = 0
    private var _packetLength = 0
    private var _state = SessionState.WaitingForLength
    private var _outputStream: DataOutputStream? = DataOutputStream(outputStream)
    private var _outputStreamLock = Object()
    private var _sentInitialMessage = false

    val id: UUID = UUID.randomUUID()

    // Not all senders send a version message to the receiver on connection. Choosing version 2
    // as the base version since most/all current senders support this version.
    var protocolVersion: Long = 2

    private fun send(opcode: Opcode, message: String? = null) {
        ensureNotMainThread()

        synchronized(_outputStreamLock) {
            try {
                val data: ByteArray = message?.encodeToByteArray() ?: ByteArray(0)
                val size = 1 + data.size
                val outputStream = _outputStream
                if (outputStream == null) {
                    Log.w(TAG, "Failed to send $size bytes, output stream is null.")
                    return
                }

                val serializedSizeLE = ByteArray(4)
                serializedSizeLE[0] = (size and 0xff).toByte()
                serializedSizeLE[1] = (size shr 8 and 0xff).toByte()
                serializedSizeLE[2] = (size shr 16 and 0xff).toByte()
                serializedSizeLE[3] = (size shr 24 and 0xff).toByte()
                outputStream.write(serializedSizeLE)

                val opcodeBytes = ByteArray(1)
                opcodeBytes[0] = opcode.value
                outputStream.write(opcodeBytes)

                if (data.isNotEmpty()) {
                    outputStream.write(data)
                }

                Log.d(TAG, "Sent $size bytes: (opcode: $opcode, body: $message).")
            } catch (e: Throwable) {
                Log.e(TAG, "$id: Failed to send message (opcode: $opcode, body: $message)", e)
                throw e
            }
        }
    }

    fun <T> send(opcode: Opcode, message: T) {
        if (!this.isSupportedOpcode(opcode)) {
            return
        }

        try {
            val strippedMessage = this.stripUnsupportedFields(opcode, message)
            when (strippedMessage) {
                is PlayMessageV1 -> send(opcode, Json.encodeToString(strippedMessage))
                is PlaybackUpdateMessageV1 -> send(opcode, Json.encodeToString(strippedMessage))
                is VolumeUpdateMessageV1 -> send(opcode, Json.encodeToString(strippedMessage))
                is PlayMessageV2 -> send(opcode, Json.encodeToString(strippedMessage))
                is PlaybackUpdateMessageV2 -> send(opcode, Json.encodeToString(strippedMessage))

                is PlayMessage -> send(
                    opcode,
                    message?.let { Json.encodeToString(it as PlayMessage) })

                is SeekMessage -> send(
                    opcode,
                    message?.let { Json.encodeToString(it as SeekMessage) })

                is PlaybackUpdateMessage -> send(
                    opcode,
                    message?.let { Json.encodeToString(it as PlaybackUpdateMessage) })

                is VolumeUpdateMessage -> send(
                    opcode,
                    message?.let { Json.encodeToString(it as VolumeUpdateMessage) })

                is SetVolumeMessage -> send(
                    opcode,
                    message?.let { Json.encodeToString(it as SetVolumeMessage) })

                is PlaybackErrorMessage -> send(
                    opcode,
                    message?.let { Json.encodeToString(it as PlaybackErrorMessage) })

                is SetSpeedMessage -> send(
                    opcode,
                    message?.let { Json.encodeToString(it as SetSpeedMessage) })

                is VersionMessage -> send(
                    opcode,
                    message?.let { Json.encodeToString(it as VersionMessage) })

                is InitialSenderMessage -> send(
                    opcode,
                    message?.let { Json.encodeToString(it as InitialSenderMessage) })

                is InitialReceiverMessage -> send(
                    opcode,
                    message?.let { Json.encodeToString(it as InitialReceiverMessage) })

                is PlayUpdateMessage -> send(
                    opcode,
                    message?.let { Json.encodeToString(it as PlayUpdateMessage) })

                is SetPlaylistItemMessage -> send(
                    opcode,
                    message?.let { Json.encodeToString(it as SetPlaylistItemMessage) })

                is SubscribeEventMessage -> send(
                    opcode,
                    message?.let { Json.encodeToString(it as SubscribeEventMessage) })

                is UnsubscribeEventMessage -> send(
                    opcode,
                    message?.let { Json.encodeToString(it as UnsubscribeEventMessage) })

                is EventMessage -> send(
                    opcode,
                    message?.let { Json.encodeToString(it as EventMessage) })

                else -> send(opcode, message?.let { Json.encodeToString(it) })
            }
        } catch (e: Throwable) {
            Log.e(TAG, "Failed to encode message to string ${id}.", e)
            throw e
        }
    }

    fun close() {
        _outputStream?.close()
    }

    fun processBytes(data: ByteArray, count: Int, offset: Int = 0) {
        if (data.isEmpty()) {
            return
        }

        Log.d(TAG, "$count bytes received from $_remoteSocketAddress")

        when (_state) {
            SessionState.WaitingForLength -> handleLengthBytes(data, offset, count)
            SessionState.WaitingForData -> handlePacketBytes(data, offset, count)
            else -> throw Exception("Invalid state $_state encountered")
        }
    }

    private fun handleLengthBytes(data: ByteArray, offset: Int, count: Int) {
        val bytesToRead = minOf(LENGTH_BYTES - _bytesRead, count)
        val bytesRemaining = count - bytesToRead
        System.arraycopy(data, offset, _buffer, _bytesRead, bytesToRead)
        _bytesRead += bytesToRead

        Log.d(TAG, "Read $bytesToRead bytes from packet")

        if (_bytesRead >= LENGTH_BYTES) {
            _state = SessionState.WaitingForData

            _packetLength = (_buffer[0].toUByte().toLong() or
                    (_buffer[1].toUByte().toLong() shl 8) or
                    (_buffer[2].toUByte().toLong() shl 16) or
                    (_buffer[3].toUByte().toLong() shl 24)).toInt()
            _bytesRead = 0

            Log.d(TAG, "Packet length header received from ${_remoteSocketAddress}: $_packetLength")

            if (_packetLength > MAXIMUM_PACKET_LENGTH) {
                Log.e(
                    TAG,
                    "Maximum packet length is 32kB, killing socket ${_remoteSocketAddress}: $_packetLength"
                )
                throw Exception("Maximum packet length is 32kB")
            }

            if (bytesRemaining > 0) {
                Log.d(
                    TAG,
                    "$bytesRemaining remaining bytes $_remoteSocketAddress pushed to handlePacketBytes"
                )
                handlePacketBytes(data, offset + bytesToRead, bytesRemaining)
            }
        }
    }

    private fun handlePacketBytes(data: ByteArray, offset: Int, count: Int) {
        val bytesToRead = minOf(_packetLength - _bytesRead, count)
        val bytesRemaining = count - bytesToRead
        System.arraycopy(data, offset, _buffer, _bytesRead, bytesToRead)
        _bytesRead += bytesToRead

        Log.d(TAG, "Read $bytesToRead bytes from packet")

        if (_bytesRead >= _packetLength) {
            Log.d(
                TAG,
                "Packet finished receiving from $_remoteSocketAddress of $_packetLength bytes."
            )
            handleNextPacket()

            _state = SessionState.WaitingForLength
            _packetLength = 0
            _bytesRead = 0

            if (bytesRemaining > 0) {
                Log.d(
                    TAG,
                    "$bytesRemaining remaining bytes $_remoteSocketAddress pushed to handleLengthBytes"
                )
                handleLengthBytes(data, offset + bytesToRead, bytesRemaining)
            }
        }
    }

    private fun handlePacket(opcode: Opcode, body: String?) {
        Log.i(
            TAG,
            "Processing packet (opcode: $opcode, size: ${body?.length ?: 0}, from ${_remoteSocketAddress})"
        )

        try {
            when (opcode) {
                Opcode.Play -> _service.onPlay(json.decodeFromString(body!!))
                Opcode.Pause -> _service.onPause()
                Opcode.Resume -> _service.onResume()
                Opcode.Stop -> _service.onStop()
                Opcode.Seek -> _service.onSeek(json.decodeFromString(body!!))
                Opcode.SetVolume -> _service.onSetVolume(json.decodeFromString(body!!))
                Opcode.SetSpeed -> _service.onSetSpeed(json.decodeFromString(body!!))
                Opcode.Version -> {
                    val versionMessage = json.decodeFromString<VersionMessage>(body!!)
                    this.protocolVersion =
                        if (versionMessage.version > 0 && versionMessage.version <= PROTOCOL_VERSION) versionMessage.version else this.protocolVersion
                    if (!this._sentInitialMessage && this.protocolVersion >= 3) {
                        this.send(
                            Opcode.Initial, InitialReceiverMessage(
                                DiscoveryService.getServiceName(),
                                NetworkService.cache.appName,
                                NetworkService.cache.appVersion,
                                NetworkService.getPlayMessage(),
                            )
                        )

                        this.send(Opcode.PlaybackUpdate, NetworkService.cache.playbackUpdate)
                        this._sentInitialMessage = true
                    }

                    _service.onVersion(json.decodeFromString(body))
                }

                Opcode.Ping -> {
                    send(Opcode.Pong)
                    _service.onPing(id)
                }

                Opcode.Pong -> _service.onPong(id)
                Opcode.Initial -> _service.onInitial(json.decodeFromString(body!!))
                Opcode.SetPlaylistItem -> _service.onSetPlaylistItem(json.decodeFromString(body!!))
                Opcode.SubscribeEvent -> _service.onSubscribeEvent(
                    id,
                    json.decodeFromString(body!!)
                )

                Opcode.UnsubscribeEvent -> _service.onUnsubscribeEvent(
                    id,
                    json.decodeFromString(body!!)
                )

                else -> {}
            }
        } catch (e: Throwable) {
            Log.e(TAG, "Failed to handle packet (opcode: ${opcode}, body: '${body}')", e)
        }
    }

    private fun handleNextPacket() {
        Log.d(TAG, "Processing packet of $_bytesRead bytes from $_remoteSocketAddress")

        val opcode = Opcode.find(_buffer[0])
        val body = if (_packetLength > 1) _buffer.copyOfRange(1, _packetLength)
            .toString(Charsets.UTF_8) else null

        Log.d(TAG, "Received packet (opcode: ${opcode}, body: '${body}')")
        handlePacket(opcode, body)
    }

    private fun isSupportedOpcode(opcode: Opcode): Boolean {
        return when (this.protocolVersion) {
            1L -> opcode.value <= 8
            2L -> opcode.value <= 13
            3L -> opcode.value <= 19
            else -> false
        }
    }

    fun <T> stripUnsupportedFields(opcode: Opcode, message: T? = null): Any? {
        return when (this.protocolVersion) {
            1L -> return when (opcode) {
                Opcode.Play -> PlayMessageV1(
                    (message as PlayMessage).container,
                    message.url,
                    message.content,
                    message.time
                )

                Opcode.PlaybackUpdate -> PlaybackUpdateMessageV1(
                    (message as PlaybackUpdateMessage).state,
                    message.time ?: 0.0
                )

                Opcode.VolumeUpdate -> VolumeUpdateMessageV1((message as VolumeUpdateMessage).volume)
                else -> message
            }

            2L -> return when (opcode) {
                Opcode.Play -> PlayMessageV2(
                    (message as PlayMessage).container,
                    message.url,
                    message.content,
                    message.time,
                    message.speed,
                    message.headers
                )

                Opcode.PlaybackUpdate -> PlaybackUpdateMessageV2(
                    (message as PlaybackUpdateMessage).generationTime,
                    message.state,
                    message.time ?: 0.0,
                    message.duration ?: 0.0,
                    message.speed ?: 1.0
                )

                else -> message
            }

            else -> message
        }
    }

    companion object {
        private const val TAG = "FCastSession"
        private val json = Json { ignoreUnknownKeys = true }
    }
}

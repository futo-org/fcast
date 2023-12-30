package com.futo.fcast.receiver

import android.util.Base64
import android.util.Log
import kotlinx.serialization.decodeFromString
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import java.io.DataOutputStream
import java.io.OutputStream
import java.math.BigInteger
import java.net.SocketAddress
import java.nio.ByteBuffer
import java.security.KeyFactory
import java.security.KeyPair
import java.security.KeyPairGenerator
import java.security.MessageDigest
import java.security.PrivateKey
import java.security.spec.X509EncodedKeySpec
import java.util.UUID
import javax.crypto.Cipher
import javax.crypto.KeyAgreement
import javax.crypto.spec.DHParameterSpec
import javax.crypto.spec.IvParameterSpec
import javax.crypto.spec.SecretKeySpec


enum class SessionState {
    Idle,
    WaitingForLength,
    WaitingForData,
    Disconnected
}

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
    Pong(13);

    companion object {
        private val _map = values().associateBy { it.value }
        fun find(value: Byte): Opcode = _map[value] ?: Opcode.None
    }
}

const val LENGTH_BYTES = 4
const val MAXIMUM_PACKET_LENGTH = 32000

class FCastSession(outputStream: OutputStream, private val _remoteSocketAddress: SocketAddress, private val _service: NetworkService) {
    private var _buffer = ByteArray(MAXIMUM_PACKET_LENGTH)
    private var _bytesRead = 0
    private var _packetLength = 0
    private var _state = SessionState.WaitingForLength
    private var _outputStream: DataOutputStream? = DataOutputStream(outputStream)
    val id = UUID.randomUUID()

    fun send(opcode: Opcode, message: String? = null) {
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
            Log.i(TAG, "Failed to send message ${id}.", e)
            throw e
        }
    }

    inline fun <reified T> send(opcode: Opcode, message: T) {
        try {
            send(opcode, message?.let { Json.encodeToString(it) })
        } catch (e: Throwable) {
            Log.i(TAG, "Failed to encode message to string ${id}.", e)
            throw e
        }
    }

    fun processBytes(data: ByteBuffer) {
        Log.i(TAG, "${data.remaining()} bytes received from ${_remoteSocketAddress}")
        if (!data.hasArray()) {
            throw IllegalArgumentException("ByteBuffer does not have a backing array")
        }

        val byteArray = data.array()
        val offset = data.arrayOffset() + data.position()
        val length = data.remaining()

        when (_state) {
            SessionState.WaitingForLength -> handleLengthBytes(byteArray, offset, length)
            SessionState.WaitingForData -> handlePacketBytes(byteArray, offset, length)
            else -> throw Exception("Invalid state $_state encountered")
        }
    }

    fun processBytes(data: ByteArray, count: Int) {
        if (data.isEmpty()) {
            return
        }

        Log.i(TAG, "$count bytes received from ${_remoteSocketAddress}")

        when (_state) {
            SessionState.WaitingForLength -> handleLengthBytes(data, 0, count)
            SessionState.WaitingForData -> handlePacketBytes(data, 0, count)
            else -> throw Exception("Invalid state $_state encountered")
        }
    }

    private fun handleLengthBytes(data: ByteArray, offset: Int, count: Int) {
        val bytesToRead = minOf(LENGTH_BYTES - _bytesRead, count)
        val bytesRemaining = count - bytesToRead
        System.arraycopy(data, offset, _buffer, _bytesRead, bytesToRead)
        _bytesRead += bytesToRead

        Log.i(TAG, "Read $bytesToRead bytes from packet")

        if (_bytesRead >= LENGTH_BYTES) {
            _state = SessionState.WaitingForData

            _packetLength = (_buffer[0].toInt() and 0xff) or
                    ((_buffer[1].toInt() and 0xff) shl 8) or
                    ((_buffer[2].toInt() and 0xff) shl 16) or
                    ((_buffer[3].toInt() and 0xff) shl 24)
            _bytesRead = 0

            Log.i(TAG, "Packet length header received from ${_remoteSocketAddress}: $_packetLength")

            if (_packetLength > MAXIMUM_PACKET_LENGTH) {
                Log.i(TAG, "Maximum packet length is 32kB, killing socket ${_remoteSocketAddress}: $_packetLength")
                throw Exception("Maximum packet length is 32kB")
            }

            if (bytesRemaining > 0) {
                Log.i(TAG, "$bytesRemaining remaining bytes ${_remoteSocketAddress} pushed to handlePacketBytes")
                handlePacketBytes(data, offset + bytesToRead, bytesRemaining)
            }
        }
    }

    private fun handlePacketBytes(data: ByteArray, offset: Int, count: Int) {
        val bytesToRead = minOf(_packetLength - _bytesRead, count)
        val bytesRemaining = count - bytesToRead
        System.arraycopy(data, offset, _buffer, _bytesRead, bytesToRead)
        _bytesRead += bytesToRead

        Log.i(TAG, "Read $bytesToRead bytes from packet")

        if (_bytesRead >= _packetLength) {
            Log.i(TAG, "Packet finished receiving from ${_remoteSocketAddress} of $_packetLength bytes.")
            handleNextPacket()

            _state = SessionState.WaitingForLength
            _packetLength = 0
            _bytesRead = 0

            if (bytesRemaining > 0) {
                Log.i(TAG, "$bytesRemaining remaining bytes ${_remoteSocketAddress} pushed to handleLengthBytes")
                handleLengthBytes(data, offset + bytesToRead, bytesRemaining)
            }
        }
    }

    private fun handleNextPacket() {
        Log.i(TAG, "Processing packet of $_bytesRead bytes from ${_remoteSocketAddress}")

        val opcode = Opcode.find(_buffer[0])
        val body = if (_packetLength > 1) _buffer.copyOfRange(1, _packetLength)
            .toString(Charsets.UTF_8) else null

        Log.i(TAG, "Received packet (opcode: ${opcode}, body: '${body}')")
        handlePacket(opcode, body)
    }

    private fun handlePacket(opcode: Opcode, body: String?) {
        Log.i(TAG, "Processing packet (opcode: $opcode, size: ${body?.length ?: 0}, from ${_remoteSocketAddress})")

        try {
            when (opcode) {
                Opcode.Play -> _service.onCastPlay(json.decodeFromString(body!!))
                Opcode.Pause -> _service.onCastPause()
                Opcode.Resume -> _service.onCastResume()
                Opcode.Stop -> _service.onCastStop()
                Opcode.Seek -> _service.onCastSeek(json.decodeFromString(body!!))
                Opcode.SetVolume -> _service.onSetVolume(json.decodeFromString(body!!))
                Opcode.SetSpeed -> _service.onSetSpeed(json.decodeFromString(body!!))
                Opcode.Ping -> send(Opcode.Pong)
                else -> { }
            }
        } catch (e: Throwable) {
            Log.e(TAG, "Failed to handle packet (opcode: ${opcode}, body: '${body}')", e)
        }
    }

    companion object {
        const val TAG = "FCastSession"
        private val json = Json { ignoreUnknownKeys = true }
    }
}
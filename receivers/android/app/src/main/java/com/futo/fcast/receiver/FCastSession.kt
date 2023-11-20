package com.futo.fcast.receiver

import android.util.Log
import kotlinx.serialization.decodeFromString
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import java.io.DataInputStream
import java.io.DataOutputStream
import java.net.Socket
import java.nio.ByteBuffer

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
    SetVolume(8)
}

const val LENGTH_BYTES = 4
const val MAXIMUM_PACKET_LENGTH = 32000

class FCastSession(private val _socket: Socket, private val _service: TcpListenerService) {
    private var _buffer = ByteArray(MAXIMUM_PACKET_LENGTH)
    private var _bytesRead = 0
    private var _packetLength = 0
    private var _state = SessionState.WaitingForLength
    private var _outputStream: DataOutputStream? = DataOutputStream(_socket.outputStream)

    fun sendPlaybackUpdate(value: PlaybackUpdateMessage) {
        send(Opcode.PlaybackUpdate, value)
    }

    fun sendVolumeUpdate(value: VolumeUpdateMessage) {
        send(Opcode.VolumeUpdate, value)
    }

    private inline fun <reified T> send(opcode: Opcode, message: T) {
        try {
            val data: ByteArray
            var jsonString: String? = null
            if (message != null) {
                jsonString = Json.encodeToString(message)
                data = jsonString.encodeToByteArray()
            } else {
                data = ByteArray(0)
            }

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

            Log.d(TAG, "Sent $size bytes: '$jsonString'.")
        } catch (e: Throwable) {
            Log.i(TAG, "Failed to send message.", e)
        }
    }

    fun processBytes(data: ByteArray, count: Int) {
        if (data.isEmpty()) {
            return
        }

        Log.i(TAG, "$count bytes received from ${_socket.remoteSocketAddress}")

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

            Log.i(TAG, "Packet length header received from ${_socket.remoteSocketAddress}: $_packetLength")

            if (_packetLength > MAXIMUM_PACKET_LENGTH) {
                Log.i(TAG, "Maximum packet length is 32kB, killing socket ${_socket.remoteSocketAddress}: $_packetLength")
                _socket.close()
                _state = SessionState.Disconnected
                return
            }

            if (bytesRemaining > 0) {
                Log.i(TAG, "$bytesRemaining remaining bytes ${_socket.remoteSocketAddress} pushed to handlePacketBytes")
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
            Log.i(TAG, "Packet finished receiving from ${_socket.remoteSocketAddress} of $_packetLength bytes.")
            handlePacket()

            _state = SessionState.WaitingForLength
            _packetLength = 0
            _bytesRead = 0

            if (bytesRemaining > 0) {
                Log.i(TAG, "$bytesRemaining remaining bytes ${_socket.remoteSocketAddress} pushed to handleLengthBytes")
                handleLengthBytes(data, offset + bytesToRead, bytesRemaining)
            }
        }
    }

    private fun handlePacket() {
        Log.i(TAG, "Processing packet of $_bytesRead bytes from ${_socket.remoteSocketAddress}")

        val opcode = Opcode.values().firstOrNull { it.value == _buffer[0] } ?: Opcode.None
        val body = if (_packetLength > 1) _buffer.copyOfRange(1, _packetLength)
            .toString(Charsets.UTF_8) else null

        Log.i(TAG, "Received packet (opcode: ${opcode}, body: '${body}')")

        try {
            when (opcode) {
                Opcode.Play -> _service.onCastPlay(Json.decodeFromString(body!!))
                Opcode.Pause -> _service.onCastPause()
                Opcode.Resume -> _service.onCastResume()
                Opcode.Stop -> _service.onCastStop()
                Opcode.Seek -> _service.onCastSeek(Json.decodeFromString(body!!))
                Opcode.SetVolume -> _service.onSetVolume(Json.decodeFromString(body!!))
                else -> { }
            }
        } catch (e: Throwable) {
            Log.e(TAG, "Failed to handle packet (opcode: ${opcode}, body: '${body}')")
        }
    }

    companion object {
        const val TAG = "FCastSession"
    }
}
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
    KeyExchange(12),
    Encrypted(13),
    Ping(14),
    Pong(15),
    StartEncryption(16);

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
    private val _keyPair: KeyPair = generateKeyPair()
    private var _aesKey: SecretKeySpec? = null
    private val _queuedEncryptedMessages = arrayListOf<EncryptedMessage>()
    private var _encryptionStarted = false
    val id = UUID.randomUUID()

    init {
        send(Opcode.KeyExchange, getKeyExchangeMessage(_keyPair))
    }

    fun sendVersion(value: VersionMessage) {
        send(Opcode.Version, value)
    }

    fun sendPlaybackError(value: PlaybackErrorMessage) {
        send(Opcode.PlaybackError, value)
    }

    fun sendPlaybackUpdate(value: PlaybackUpdateMessage) {
        send(Opcode.PlaybackUpdate, value)
    }

    fun sendVolumeUpdate(value: VolumeUpdateMessage) {
        send(Opcode.VolumeUpdate, value)
    }

    private fun send(opcode: Opcode, message: String? = null) {
        val aesKey = _aesKey
        if (_encryptionStarted && aesKey != null && opcode != Opcode.Encrypted && opcode != Opcode.KeyExchange && opcode != Opcode.StartEncryption) {
            send(Opcode.Encrypted, encryptMessage(aesKey, DecryptedMessage(opcode.value.toLong(), message)))
            return
        }

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

    private inline fun <reified T> send(opcode: Opcode, message: T) {
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
                Opcode.KeyExchange -> {
                    val keyExchangeMessage: KeyExchangeMessage = json.decodeFromString(body!!)
                    _aesKey = computeSharedSecret(_keyPair.private, keyExchangeMessage)
                    send(Opcode.StartEncryption)

                    synchronized(_queuedEncryptedMessages) {
                        for (queuedEncryptedMessages in _queuedEncryptedMessages) {
                            val decryptedMessage = decryptMessage(_aesKey!!, queuedEncryptedMessages)
                            val o = Opcode.find(decryptedMessage.opcode.toByte())
                            handlePacket(o, decryptedMessage.message)
                        }

                        _queuedEncryptedMessages.clear()
                    }
                }
                Opcode.Ping -> send(Opcode.Pong)
                Opcode.Encrypted -> {
                    val encryptedMessage: EncryptedMessage = json.decodeFromString(body!!)
                    if (_aesKey != null) {
                        val decryptedMessage = decryptMessage(_aesKey!!, encryptedMessage)
                        val o = Opcode.find(decryptedMessage.opcode.toByte())
                        handlePacket(o, decryptedMessage.message)
                    } else {
                        synchronized(_queuedEncryptedMessages) {
                            if (_queuedEncryptedMessages.size == 15) {
                                _queuedEncryptedMessages.removeAt(0)
                            }

                            _queuedEncryptedMessages.add(encryptedMessage)
                        }
                    }
                }
                Opcode.StartEncryption -> {
                    _encryptionStarted = true
                    //TODO: Send decrypted messages waiting for encryption to be established
                }
                else -> { }
            }
        } catch (e: Throwable) {
            Log.e(TAG, "Failed to handle packet (opcode: ${opcode}, body: '${body}')", e)
        }
    }

    companion object {
        const val TAG = "FCastSession"
        private val json = Json { ignoreUnknownKeys = true }

        fun getKeyExchangeMessage(keyPair: KeyPair): KeyExchangeMessage {
            return KeyExchangeMessage(1, Base64.encodeToString(keyPair.public.encoded, Base64.NO_WRAP))
        }

        fun generateKeyPair(): KeyPair {
            //modp14
            val p = BigInteger("ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f14374fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7edee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf0598da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb9ed529077096966d670c354e4abc9804f1746c08ca18217c32905e462e36ce3be39e772c180e86039b2783a2ec07a28fb5c55df06f4c52c9de2bcbf6955817183995497cea956ae515d2261898fa051015728e5a8aacaa68ffffffffffffffff", 16)
            val g = BigInteger("2", 16)
            val dhSpec = DHParameterSpec(p, g)

            val keyGen = KeyPairGenerator.getInstance("DH")
            keyGen.initialize(dhSpec)

            return keyGen.generateKeyPair()
        }

        fun computeSharedSecret(privateKey: PrivateKey, keyExchangeMessage: KeyExchangeMessage): SecretKeySpec {
            val keyFactory = KeyFactory.getInstance("DH")
            val receivedPublicKeyBytes = Base64.decode(keyExchangeMessage.publicKey, Base64.NO_WRAP)
            val receivedPublicKeySpec = X509EncodedKeySpec(receivedPublicKeyBytes)
            val receivedPublicKey = keyFactory.generatePublic(receivedPublicKeySpec)

            val keyAgreement = KeyAgreement.getInstance("DH")
            keyAgreement.init(privateKey)
            keyAgreement.doPhase(receivedPublicKey, true)

            val sharedSecret = keyAgreement.generateSecret()
            Log.i(TAG, "sharedSecret ${Base64.encodeToString(sharedSecret, Base64.NO_WRAP)}")
            val sha256 = MessageDigest.getInstance("SHA-256")
            val hashedSecret = sha256.digest(sharedSecret)
            Log.i(TAG, "hashedSecret ${Base64.encodeToString(hashedSecret, Base64.NO_WRAP)}")

            return SecretKeySpec(hashedSecret, "AES")
        }

        fun encryptMessage(aesKey: SecretKeySpec, decryptedMessage: DecryptedMessage): EncryptedMessage {
            val cipher = Cipher.getInstance("AES/CBC/PKCS5Padding")
            cipher.init(Cipher.ENCRYPT_MODE, aesKey)
            val iv = cipher.iv
            val json = Json.encodeToString(decryptedMessage)
            val encrypted = cipher.doFinal(json.toByteArray(Charsets.UTF_8))
            return EncryptedMessage(
                version = 1,
                iv = Base64.encodeToString(iv, Base64.NO_WRAP),
                blob = Base64.encodeToString(encrypted, Base64.NO_WRAP)
            )
        }

        fun decryptMessage(aesKey: SecretKeySpec, encryptedMessage: EncryptedMessage): DecryptedMessage {
            val iv = Base64.decode(encryptedMessage.iv, Base64.NO_WRAP)
            val encrypted = Base64.decode(encryptedMessage.blob, Base64.NO_WRAP)

            val cipher = Cipher.getInstance("AES/CBC/PKCS5Padding")
            cipher.init(Cipher.DECRYPT_MODE, aesKey, IvParameterSpec(iv))
            val decryptedJson = cipher.doFinal(encrypted)
            return Json.decodeFromString(String(decryptedJson, Charsets.UTF_8))
        }
    }
}
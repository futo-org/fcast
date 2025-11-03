// Initially based on the android implementation in https://github.com/software-mansion/react-native-whip-whep released under the MIT license

package com.futo.fcast.receiver

import android.content.Context
import android.util.Log
import androidx.annotation.OptIn
import androidx.media3.common.util.UnstableApi
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.MainScope
import kotlinx.coroutines.launch
import okhttp3.Call
import okhttp3.Callback
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response
import org.json.JSONObject
import org.webrtc.AudioTrack
import org.webrtc.DataChannel
import org.webrtc.DefaultVideoDecoderFactory
import org.webrtc.EglBase
import org.webrtc.IceCandidate
import org.webrtc.MediaConstraints
import org.webrtc.MediaStream
import org.webrtc.MediaStreamTrack
import org.webrtc.PeerConnection
import org.webrtc.PeerConnectionFactory
import org.webrtc.RtpReceiver
import org.webrtc.RtpTransceiver
import org.webrtc.SdpObserver
import org.webrtc.SessionDescription
import org.webrtc.VideoTrack
import java.io.IOException
import java.net.ConnectException
import java.net.URI
import java.net.URL
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException
import kotlin.coroutines.suspendCoroutine

interface ClientBaseListener {
    fun onTrackAdded(track: VideoTrack)
}

class WhepClient(appContext: Context, eglBase: EglBase) : PeerConnection.Observer {
    val peerConnectionFactory = getPeerConnectionFactory(appContext, eglBase)
    var peerConnection: PeerConnection? = null
    var serverUrl: String? = null

    var patchEndpoint: String? = null
    val client = OkHttpClient().newBuilder().retryOnConnectionFailure(true).build()
    val coroutineScope: CoroutineScope = CoroutineScope(Dispatchers.Default)
    var videoTrack: VideoTrack? = null
    var audioTrack: AudioTrack? = null
    private var listeners = mutableListOf<ClientBaseListener>()
    var onTrackAdded: (() -> Unit)? = null
    var onConnectionStateChanged: ((PeerConnection.PeerConnectionState) -> Unit)? = null

    fun setupPeerConnection() {
        val config = PeerConnection.RTCConfiguration(listOf())

        config.sdpSemantics = PeerConnection.SdpSemantics.UNIFIED_PLAN
        config.continualGatheringPolicy = PeerConnection.ContinualGatheringPolicy.GATHER_CONTINUALLY
        config.candidateNetworkPolicy = PeerConnection.CandidateNetworkPolicy.ALL
        config.tcpCandidatePolicy = PeerConnection.TcpCandidatePolicy.DISABLED

        try {
            peerConnection = peerConnectionFactory.createPeerConnection(config, this)!!
        } catch (_: NullPointerException) {
            throw Exception("Failed to establish RTCPeerConnection. Check initial configuration")
        }
    }

    suspend fun sendSdpOffer(sdpOffer: String) = suspendCoroutine { continuation ->
        if (serverUrl == null) {
            continuation.resumeWithException(
                Exception("Cannot send the SDP Offer. Connection not setup. Remember to call connect first.")
            )
            return@suspendCoroutine
        }

        Log.d("Testing", "Connecting to server url: $serverUrl")
        val request = Request.Builder().url(serverUrl!!).post(sdpOffer.toRequestBody())
            .header("Accept", "application/sdp").header("Content-Type", "application/sdp").build()

        client.newCall(request).enqueue(object : Callback {
            override fun onFailure(
                call: Call, e: IOException
            ) {
                if (e is ConnectException) {
                    continuation.resumeWithException(
                        Exception(
                            "Network error. Check if the server is up and running and the token and the server url is correct."
                        )
                    )
                } else {
                    Log.e(TAG, e.toString())
                    continuation.resumeWithException(e)
                    e.printStackTrace()
                }
            }

            override fun onResponse(
                call: Call, response: Response
            ) {
                response.use {
                    patchEndpoint = response.headers["location"]

                    if (patchEndpoint == null) {
                        continuation.resumeWithException(
                            Exception("Location attribute not found. Check if the SDP answer contains location parameter.")
                        )
                        return
                    }

                    continuation.resume(response.body.string())
                }
            }
        })
    }

    override fun onSignalingChange(p0: PeerConnection.SignalingState?) {
        Log.d(TAG, "RTC signaling state changed:: ${p0?.name}")
    }

    override fun onIceConnectionReceivingChange(p0: Boolean) {
        Log.d(TAG, "onIceConnectionReceivingChange: $p0")
    }

    override fun onIceGatheringChange(p0: PeerConnection.IceGatheringState?) {
        Log.d(TAG, "RTC ICE gathering state changed: ${p0?.name}")
    }

    override fun onIceCandidate(candidate: IceCandidate) {
        Log.i(TAG, "onIceCandidate: $candidate")
    }

    override fun onIceCandidatesRemoved(p0: Array<out IceCandidate>?) {
        Log.d(TAG, "Removed candidate from candidates list.")
    }

    override fun onAddStream(p0: MediaStream?) {
        Log.d(TAG, "RTC media stream added: ${p0?.id}")
    }

    override fun onRemoveStream(p0: MediaStream?) {
        Log.d(TAG, "RTC media stream removed: ${p0?.id}")
    }

    override fun onDataChannel(p0: DataChannel?) {
        Log.d(TAG, "RTC data channel opened: ${p0?.id()}")
    }

    override fun onRenegotiationNeeded() {
        Log.d(TAG, "Peer connection negotiation needed.")
    }

    override fun onConnectionChange(newState: PeerConnection.PeerConnectionState) {
        onConnectionStateChanged?.invoke(newState)
        when (newState) {
            PeerConnection.PeerConnectionState.NEW -> Log.d(TAG, "New connection")
            PeerConnection.PeerConnectionState.CONNECTING -> Log.d(TAG, "Connecting")
            PeerConnection.PeerConnectionState.CONNECTED -> Log.d(
                TAG, "Connection is fully connected"
            )

            PeerConnection.PeerConnectionState.DISCONNECTED -> Log.d(
                TAG, "One or more transports has disconnected unexpectedly"
            )

            PeerConnection.PeerConnectionState.FAILED -> Log.d(
                TAG, "One or more transports has encountered an error"
            )

            PeerConnection.PeerConnectionState.CLOSED -> Log.d(
                TAG, "Connection has been closed"
            )
        }
    }

    override fun onAddTrack(
        receiver: RtpReceiver?, mediaStreams: Array<out MediaStream>?
    ) {
        coroutineScope.launch(Dispatchers.Main) {
            val addedVideoTrack = receiver?.track() as? VideoTrack?
            Log.i(TAG, "Video track was added")
            videoTrack = addedVideoTrack
            listeners.forEach { listener -> videoTrack?.let { listener.onTrackAdded(it) } }
        }
        onTrackAdded?.let { it() }
    }

    fun addTrackListener(listener: ClientBaseListener) {
        listeners.add(listener)
        videoTrack?.let { listener.onTrackAdded(it) }
    }

    fun connect(serverUrl: String) {
        this.serverUrl = serverUrl

        if (peerConnection == null) {
            setupPeerConnection()
        }

        val videoTransceiver =
            peerConnection?.addTransceiver(MediaStreamTrack.MediaType.MEDIA_TYPE_VIDEO)
        videoTransceiver?.direction = RtpTransceiver.RtpTransceiverDirection.RECV_ONLY

        val audioTransceiver =
            peerConnection?.addTransceiver(MediaStreamTrack.MediaType.MEDIA_TYPE_AUDIO)
        audioTransceiver?.direction = RtpTransceiver.RtpTransceiverDirection.RECV_ONLY

        open class TestObserver() : SdpObserver {
            override fun onCreateSuccess(sdp: SessionDescription?) {}

            override fun onSetSuccess() {}

            override fun onCreateFailure(error: String?) {}

            override fun onSetFailure(error: String?) {}
        }

        val constraints = MediaConstraints()

        val observer = object : TestObserver() {
            override fun onCreateSuccess(sdp: SessionDescription?) {
                Log.i(TAG, "On create success: $sdp")
                peerConnection?.setLocalDescription(object : TestObserver() {
                    @OptIn(UnstableApi::class)
                    override fun onSetSuccess() {
                        MainScope().launch {
                            val localSdp =
                                peerConnection?.localDescription?.description ?: sdp?.description
                                ?: return@launch
                            val sdpResp = try {
                                 sendSdpOffer(localSdp)
                            } catch (e: Exception) {
                                Log.e(TAG, "Failed to send SDP offer: $e")
                                PlayerActivity.instance?.viewModel?.errorMessage = "Failed to send SDP offer: $e"
                                return@launch
                            }

                            val answer = SessionDescription(
                                SessionDescription.Type.ANSWER, sdpResp
                            )

                            Log.i(
                                TAG, "Signalling state: ${peerConnection?.signalingState()}"
                            )
                            peerConnection?.setRemoteDescription(object : TestObserver() {
                                override fun onSetSuccess() {
                                    Log.i(TAG, "All OK")
                                }

                                override fun onSetFailure(err: String?) {
                                    Log.e(TAG, "Failed to set remote description: $err")
                                }
                            }, answer)
                        }
                    }
                }, sdp)
            }
        }

        peerConnection!!.createOffer(observer, constraints)
    }

    fun disconnect() {
        peerConnection?.close()
        peerConnection?.dispose()
        peerConnection = null
        patchEndpoint = null
        videoTrack = null
        audioTrack = null
    }

    override fun onIceConnectionChange(connectionState: PeerConnection.IceConnectionState?) {
        Log.i(TAG, "ICE connection changed: $connectionState")
    }

    companion object {
        const val TAG = "WHEPClient"

        private var peerConnectionFactory: PeerConnectionFactory? = null

        fun getPeerConnectionFactory(
            appContext: Context, eglBase: EglBase
        ): PeerConnectionFactory {
            if (peerConnectionFactory == null) {
                PeerConnectionFactory.initialize(
                    PeerConnectionFactory.InitializationOptions.builder(appContext)
                        .createInitializationOptions()
                )
                return PeerConnectionFactory.builder()
                    .setVideoDecoderFactory(DefaultVideoDecoderFactory(eglBase.eglBaseContext))
                    .createPeerConnectionFactory()
            }
            return peerConnectionFactory!!
        }
    }
}

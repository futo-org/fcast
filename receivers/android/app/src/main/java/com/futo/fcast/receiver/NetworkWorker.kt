package com.futo.fcast.receiver

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.os.Build
import android.util.Log
import java.net.NetworkInterface

class NetworkWorker(private val _context: Context) {
    private val _connectivityManager =
        _context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager

    init {
        val networkRequest = NetworkRequest.Builder().build()
        val networkCallback = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                super.onAvailable(network)
            }

            override fun onLost(network: Network) {
                super.onLost(network)
            }

            override fun onCapabilitiesChanged(
                network: Network,
                networkCapabilities: NetworkCapabilities
            ) {
                super.onCapabilitiesChanged(network, networkCapabilities)
            }
        }

        _connectivityManager.registerNetworkCallback(networkRequest, networkCallback)
//        connectivityManager.unregisterNetworkCallback(networkCallback)
    }

    private fun getActiveNetworkInterfaces(): List<NetworkInterface> {
        val activeInterfaces = mutableListOf<NetworkInterface>()
        try {
            val interfaces = NetworkInterface.getNetworkInterfaces()
            interfaces?.let {
                for (networkInterface in interfaces) {
                    if (networkInterface.isUp && !networkInterface.isLoopback && !networkInterface.isVirtual) {
                        activeInterfaces.add(networkInterface)
                    }
                }
            }
        } catch (e: Exception) {

        }
        return activeInterfaces
    }


    fun getNetworkInfo(): List<NetworkInterfaceData> {
        val activeInterfaces = getActiveNetworkInterfaces()
        val connectedInterfaces = mutableListOf<NetworkInterfaceData>()

        for (iface in activeInterfaces) {
            val network = getNetworkForInterface(iface)

            if (network != null) {
                val capabilities = _connectivityManager.getNetworkCapabilities(network)
                if (capabilities != null) {
                    val type = when {
                        capabilities.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) -> NetworkInterfaceType.Wireless
                        capabilities.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) -> NetworkInterfaceType.Wired
                        capabilities.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) -> NetworkInterfaceType.Wireless
                        else -> NetworkInterfaceType.Unknown
                    }

                    for (addr in iface.inetAddresses) {
                        if (addr.isLoopbackAddress) {
                            continue
                        }

                        if (addr.address.size != 4) {
                            continue
                        }

                        Log.i(
                            TAG,
                            "Running on ${addr.hostAddress}:${TcpListenerService.PORT} (TCP)"
                        )
                        Log.i(
                            TAG,
                            "Running on ${addr.hostAddress}:${WebSocketListenerService.PORT} (WebSocket)"
                        )

                        // todo: determine normalized rssi value range
                        val signalStrength = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                            if (type == NetworkInterfaceType.Wireless) capabilities.signalStrength else null
                        } else null

                        addr.hostAddress?.let {
                            connectedInterfaces.add(
                                NetworkInterfaceData(
                                    type,
                                    iface.displayName,
                                    it,
                                    signalStrength
                                )
                            )
                        }

                    }
                }
            }
        }
        return connectedInterfaces
    }

    private fun getNetworkForInterface(iface: NetworkInterface): Network? {
        for (network in _connectivityManager.allNetworks) {
            val linkProperties = _connectivityManager.getLinkProperties(network)
            if (linkProperties != null) {
                for (linkAddress in linkProperties.linkAddresses) {
                    val networkInterface = NetworkInterface.getByInetAddress(linkAddress.address)
                    if (networkInterface != null && networkInterface.name == iface.name) {
                        return network
                    }
                }
            }
        }

        return null
    }

//    fun getIPs(): List<String> {
//        val ips = arrayListOf<String>()
//
//        for (iface in NetworkInterface.getNetworkInterfaces()) {
//            if (!iface.isUp ||  iface.isVirtual || iface.isLoopback) {
//                continue
//            }
//
//
//            for (addr in iface.inetAddresses) {
//                if (addr.isLoopbackAddress) {
//                    continue
//                }
//
//                if (addr.address.size != 4) {
//                    continue
//                }
//
//                Log.i(TAG, "Running on ${addr.hostAddress}:${TcpListenerService.PORT} (TCP)")
//                Log.i(TAG, "Running on ${addr.hostAddress}:${WebSocketListenerService.PORT} (WebSocket)")
//                addr.hostAddress?.let { ips.add(it) }
//            }
//        }
//        return ips
//    }

    companion object {
        private const val TAG = "NetworkWorker"
    }
}

enum class NetworkInterfaceType {
    Wired,
    Wireless,
    Unknown
}

data class NetworkInterfaceData(
    val type: NetworkInterfaceType,
    val name: String,
    val address: String,
    val signalLevel: Int?
)

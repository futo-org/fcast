package com.futo.fcast.receiver

import android.annotation.SuppressLint
import android.content.Context
import android.net.ConnectivityManager
import android.net.LinkAddress
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.wifi.WifiManager
import android.net.wifi.WifiManager.WifiLock
import android.os.Build
import android.os.PowerManager
import android.os.PowerManager.WakeLock
import android.util.Log
import android.widget.Toast
import androidx.annotation.OptIn
import androidx.media3.common.util.UnstableApi
import java.net.NetworkInterface

class NetworkWorker(private val _context: Context) {
    private val _connectivityManager =
        _context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
    private val _wifiManager =
        _context.applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
    private var _stopped: Boolean = true

    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        @OptIn(UnstableApi::class)
        override fun onAvailable(network: Network) {
            val linkProperties = _connectivityManager.getLinkProperties(network)
            Log.i(
                TAG,
                "New network interface available: ${linkProperties?.interfaceName} ${linkProperties?.linkAddresses}"
            )
            PlayerActivity.instance?.onNetworkConnectionAvailable()

            if (linkProperties?.interfaceName != null) {
                val iface = NetworkInterface.getByName(linkProperties.interfaceName)
                if (iface != null) {
                    val added = addNetwork(iface, network)

                    if (added) {
                        MainActivity.instance?.networkChanged()
                        Toast.makeText(
                            _context,
                            _context.getString(R.string.network_changed),
                            Toast.LENGTH_LONG
                        ).show()
                    }
                }
            } else {
                Log.w(TAG, "Could not find interface from network object")
            }
        }

        override fun onLost(network: Network) {
            val linkProperties = _connectivityManager.getLinkProperties(network)
            Log.i(
                TAG,
                "Network interface lost: ${linkProperties?.interfaceName} ${linkProperties?.linkAddresses}"
            )

            if (linkProperties?.linkAddresses != null) {
                val removed = removeNetwork(linkProperties.linkAddresses)

                if (removed) {
                    MainActivity.instance?.networkChanged()

                    if (interfaces.isEmpty()) {
                        Toast.makeText(
                            _context,
                            _context.getString(R.string.network_lost),
                            Toast.LENGTH_LONG
                        ).show()
                    } else {
                        Toast.makeText(
                            _context,
                            _context.getString(R.string.network_changed),
                            Toast.LENGTH_LONG
                        ).show()
                    }
                }
            } else {
                Log.w(TAG, "Could not find interface from network object")
            }
        }
    }

    val interfaces = mutableListOf<NetworkInterfaceData>()

    fun start() {
        Log.i(TAG, "Starting $TAG")
        if (!_stopped) {
            return
        }
        _stopped = false

        val networkRequest = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
            .addTransportType(NetworkCapabilities.TRANSPORT_ETHERNET)
            .addTransportType(NetworkCapabilities.TRANSPORT_CELLULAR)
            .build()

        val activeInterfaces = getActiveNetworkInterfaces()

        for (iface in activeInterfaces) {
            val network = getNetworkForInterface(iface)

            if (network != null) {
                addNetwork(iface, network)
            }
        }

        _connectivityManager.registerNetworkCallback(networkRequest, networkCallback)
        Log.i(TAG, "Started $TAG")
    }

    fun stop() {
        Log.i(TAG, "Stopping $TAG")
        if (_stopped) {
            return
        }
        _stopped = true
        _connectivityManager.unregisterNetworkCallback(networkCallback)

        Log.i(TAG, "Stopped $TAG")
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
            Log.e(TAG, "Error querying network interfaces: ${e.message}")
        }
        return activeInterfaces
    }

    private fun addNetwork(iface: NetworkInterface, network: Network): Boolean {
        var added = false
        val capabilities = _connectivityManager.getNetworkCapabilities(network)

        if (capabilities != null) {
            val (type, displayName) = when {
                capabilities.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) -> Pair(
                    NetworkInterfaceType.Wireless,
                    "Wi-Fi"
                )

                capabilities.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) -> Pair(
                    NetworkInterfaceType.Wired,
                    "Wired"
                )

                capabilities.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) -> Pair(
                    NetworkInterfaceType.Wireless,
                    "Data"
                )

                else -> Pair(NetworkInterfaceType.Unknown, iface.displayName)
            }

            for (address in iface.inetAddresses) {
                if (address.isLoopbackAddress || address.address.size != 4) {
                    continue
                }
                Log.i(TAG, "Adding address ${address.hostAddress} to interface list")

                // Note: Holding off on real-time signal strength and SSID querying due to requiring location permissions
                val signalStrength = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                    if (type == NetworkInterfaceType.Wireless) _wifiManager.calculateSignalLevel(
                        capabilities.signalStrength
                    ) else null
                } else null

                address.hostAddress?.let { it ->
                    if (interfaces.find { it.address == address.hostAddress } != null) {
                        Log.i(TAG, "Already added interface $it")
                    } else {
                        interfaces.add(
                            NetworkInterfaceData(
                                type,
                                displayName,
                                it,
                                signalStrength
                            )
                        )
                        added = true
                    }
                }
            }
        }

        return added
    }

    private fun removeNetwork(addresses: List<LinkAddress>): Boolean {
        val initialSize = interfaces.size

        for (addr in addresses) {
            interfaces.removeIf { it.address == addr.address.hostAddress }
        }

        return initialSize != interfaces.size
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

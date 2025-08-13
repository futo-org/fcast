package org.fcast.sender_sdk

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import android.util.Log
import java.net.Inet4Address
import java.net.Inet6Address
import java.net.InetAddress

private fun inetAddressToIpAddr(addrs: Array<InetAddress>): List<IpAddr> {
    return addrs.map { addr ->
        val bytes = addr.address
        if (addr is Inet4Address) {
            return@map IpAddr.V4(
                bytes[0].toUByte(),
                bytes[1].toUByte(),
                bytes[2].toUByte(),
                bytes[3].toUByte()
            )
        }
        if (addr is Inet6Address) {
            return@map IpAddr.V6(
                bytes[0].toUByte(), bytes[1].toUByte(), bytes[2].toUByte(), bytes[3].toUByte(),
                bytes[4].toUByte(), bytes[5].toUByte(), bytes[6].toUByte(), bytes[7].toUByte(),
                bytes[8].toUByte(), bytes[9].toUByte(), bytes[10].toUByte(), bytes[11].toUByte(),
                bytes[12].toUByte(), bytes[13].toUByte(), bytes[14].toUByte(), bytes[15].toUByte(),
                addr.scopeId.toUInt()
            )
        } else {
            throw IllegalStateException("Invalid InetAddress")
        }
    }
}

class NsdDeviceDiscoverer {
    private var nsdManager: NsdManager
    private val devices: HashSet<String> = hashSetOf()
    private val eventHandler: DeviceDiscovererEventHandler
    private val discoveryListeners = mapOf(
        "_googlecast._tcp" to createDiscoveryListener(::chromecastDeviceEvent),
        "_fcast._tcp" to createDiscoveryListener(::fCastDeviceEvent)
    )

    constructor(context: Context, discovererEventHandler: DeviceDiscovererEventHandler) {
        eventHandler = discovererEventHandler
        nsdManager = context.getSystemService(Context.NSD_SERVICE) as NsdManager
        nsdManager.apply {
            discoveryListeners.forEach {
                discoverServices(it.key, NsdManager.PROTOCOL_DNS_SD, it.value)
            }
        }
    }

    private fun createDiscoveryListener(
        addOrUpdate: (String, List<IpAddr>, UShort, Map<String, ByteArray>, lost: Boolean) -> Unit
    ): NsdManager.DiscoveryListener {
        return object : NsdManager.DiscoveryListener {
            override fun onDiscoveryStarted(regType: String) {
                Log.d(TAG, "Service discovery started for $regType")
            }

            override fun onDiscoveryStopped(serviceType: String) {
                Log.i(TAG, "Discovery stopped: $serviceType")
            }

            override fun onServiceLost(service: NsdServiceInfo) {
                Log.e(TAG, "Service lost: $service")
                val addresses = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                    service.hostAddresses.toTypedArray()
                } else {
                    when (service.host) {
                        null -> arrayOf()
                        else -> arrayOf(service.host)
                    }
                }
                addOrUpdate(
                    service.serviceName,
                    inetAddressToIpAddr(addresses),
                    service.port.toUShort(),
                    service.attributes,
                    true,
                )
            }

            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                Log.e(TAG, "Discovery failed for $serviceType: Error code:$errorCode")
                try {
                    nsdManager.stopServiceDiscovery(this)
                } catch (e: Throwable) {
                    Log.w(TAG, "Failed to stop service discovery", e)
                }
            }

            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {
                Log.e(TAG, "Stop discovery failed for $serviceType: Error code:$errorCode")
                try {
                    nsdManager.stopServiceDiscovery(this)
                } catch (e: Throwable) {
                    Log.w(TAG, "Failed to stop service discovery", e)
                }
            }

            override fun onServiceFound(service: NsdServiceInfo) {
                Log.v(TAG, "Service discovery success for ${service.serviceType}: $service")
                val addresses = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                    service.hostAddresses.toTypedArray()
                } else {
                    when (service.host) {
                        null -> arrayOf()
                        else -> arrayOf(service.host)
                    }
                }
                addOrUpdate(
                    service.serviceName,
                    inetAddressToIpAddr(addresses),
                    service.port.toUShort(),
                    service.attributes,
                    false,
                )
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                    nsdManager.registerServiceInfoCallback(
                        service,
                        { it.run() },
                        object : NsdManager.ServiceInfoCallback {
                            override fun onServiceUpdated(serviceInfo: NsdServiceInfo) {
                                Log.v(TAG, "onServiceUpdated: $serviceInfo")
                                addOrUpdate(
                                    serviceInfo.serviceName,
                                    inetAddressToIpAddr(serviceInfo.hostAddresses.toTypedArray()),
                                    serviceInfo.port.toUShort(),
                                    serviceInfo.attributes,
                                    false,
                                )
                            }

                            override fun onServiceLost() {
                                Log.v(TAG, "onServiceLost: $service")
                            }

                            override fun onServiceInfoCallbackRegistrationFailed(errorCode: Int) {
                                Log.v(TAG, "onServiceInfoCallbackRegistrationFailed: $errorCode")
                            }

                            override fun onServiceInfoCallbackUnregistered() {
                                Log.v(TAG, "onServiceInfoCallbackUnregistered")
                            }
                        })
                } else {
                    nsdManager.resolveService(service, object : NsdManager.ResolveListener {
                        override fun onResolveFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {
                            Log.v(TAG, "Resolve failed: $errorCode")
                        }

                        override fun onServiceResolved(serviceInfo: NsdServiceInfo) {
                            Log.v(TAG, "Resolve Succeeded: $serviceInfo")
                            serviceInfo.host?.let { hostAddr ->
                                addOrUpdate(
                                    serviceInfo.serviceName,
                                    inetAddressToIpAddr(arrayOf(hostAddr)),
                                    serviceInfo.port.toUShort(),
                                    serviceInfo.attributes,
                                    false,
                                )
                            }
                        }
                    })
                }
            }
        }
    }

    private fun chromecastDeviceEvent(
        name: String,
        addresses: List<IpAddr>,
        port: UShort,
        txt: Map<String, ByteArray>,
        lost: Boolean,
    ) {
        val fullName = "$name._googlecast._tcp"
        txt["fn"]?.let {
            val friendlyName = it.decodeToString()
            if (lost) {
                eventHandler.deviceRemoved(friendlyName)
                devices.remove(fullName)
                return
            }

            val deviceInfo =
                DeviceInfo(friendlyName, ProtocolType.CHROMECAST, addresses, port)
            if (devices.contains(fullName)) {
                eventHandler.deviceChanged(deviceInfo)
            } else {
                eventHandler.deviceAvailable(deviceInfo)
                devices.add(fullName)
            }
        }
    }

    private fun fCastDeviceEvent(
        name: String,
        addresses: List<IpAddr>,
        port: UShort,
        txt: Map<String, ByteArray>,
        lost: Boolean,
    ) {
        val fullName = "$name._fcast._tcp"
        if (lost) {
            eventHandler.deviceRemoved(name)
            devices.remove(fullName)
            return
        }
        val deviceInfo = DeviceInfo(name, ProtocolType.F_CAST, addresses, port)
        if (devices.contains(fullName)) {
            eventHandler.deviceChanged(deviceInfo)
        } else {
            eventHandler.deviceAvailable(deviceInfo)
            devices.add(fullName)
        }
    }

    companion object {
        private val TAG = "NsdDeviceDiscoverer"
    }
}

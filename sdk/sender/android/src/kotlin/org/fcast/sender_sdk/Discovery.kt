package org.fcast.sender_sdk

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import java.net.Inet4Address
import java.net.Inet6Address
import java.net.InetAddress
import java.util.concurrent.atomic.AtomicBoolean

private fun inetAddressToIpAddr(addrs: Array<InetAddress>): List<IpAddr> {
    return addrs.mapNotNull { addr ->
        val bytes = addr.address
        when (addr) {
            is Inet4Address -> IpAddr.V4(
                bytes[0].toUByte(),
                bytes[1].toUByte(),
                bytes[2].toUByte(),
                bytes[3].toUByte()
            )
            is Inet6Address -> IpAddr.V6(
                bytes[0].toUByte(), bytes[1].toUByte(), bytes[2].toUByte(), bytes[3].toUByte(),
                bytes[4].toUByte(), bytes[5].toUByte(), bytes[6].toUByte(), bytes[7].toUByte(),
                bytes[8].toUByte(), bytes[9].toUByte(), bytes[10].toUByte(), bytes[11].toUByte(),
                bytes[12].toUByte(), bytes[13].toUByte(), bytes[14].toUByte(), bytes[15].toUByte(),
                addr.scopeId.toUInt()
            )
            // unreachable with the current framework, but a throw here would
            // kill the NSD handler thread and the app with it, so skip instead
            else -> null
        }
    }
}

// NsdServiceInfo.getAttributes() maps a TXT key that carries no value to a null
// ByteArray, so entries are nullable even though the platform type hides it.
// Valueless keys become "", matching what the desktop mDNS backend reports.
private fun decodeTxtRecords(txt: Map<String, ByteArray?>): Map<String, String> {
    return txt.mapValues { it.value?.decodeToString() ?: "" }
}

private typealias DeviceEventFn =
    (String, List<IpAddr>, UShort, Map<String, ByteArray?>, lost: Boolean) -> Unit

class NsdDeviceDiscoverer {
    private var nsdManager: NsdManager
    // mDNS fullname to the name reported through the event handler. Lost events
    // often carry no TXT records, so the reported name must be remembered here.
    private val devices: HashMap<String, String> = hashMapOf()
    private val eventHandler: DeviceDiscovererEventHandler
    // Pre-34 NsdManager allows one outstanding resolve per app, overlapping
    // calls fail with FAILURE_ALREADY_ACTIVE. Resolves are serialized here.
    private val resolveQueue = ArrayDeque<PendingResolve>()
    private var resolveInFlight = false
    private val retryHandler = Handler(Looper.getMainLooper())
    private val discoveryListeners = mapOf(
        "_googlecast._tcp" to createDiscoveryListener(::chromecastDeviceEvent),
        "_fcast._tcp" to createDiscoveryListener(::fCastDeviceEvent)
    )

    private class PendingResolve(
        val service: NsdServiceInfo,
        val addOrUpdate: DeviceEventFn,
        var attempts: Int = 0,
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

    private fun createDiscoveryListener(addOrUpdate: DeviceEventFn): NsdManager.DiscoveryListener {
        return object : NsdManager.DiscoveryListener {
            override fun onDiscoveryStarted(regType: String) {
                Log.d(TAG, "Service discovery started for $regType")
            }

            override fun onDiscoveryStopped(serviceType: String) {
                Log.i(TAG, "Discovery stopped: $serviceType")
            }

            override fun onServiceLost(service: NsdServiceInfo) {
                Log.e(TAG, "Service lost: $service")
                // serviceName is a platform type and nullable in the framework
                val serviceName = service.serviceName ?: return
                val addresses = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                    service.hostAddresses.toTypedArray()
                } else {
                    when (service.host) {
                        null -> arrayOf()
                        else -> arrayOf(service.host)
                    }
                }
                addOrUpdate(
                    serviceName,
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
                // resolveService and registerServiceInfoCallback throw on a
                // null or empty name, guard like the other callbacks
                if (service.serviceName.isNullOrEmpty()) return
                // Do NOT report the device here. At onServiceFound the TXT records
                // (which carry the `fp` crypto fingerprint needed for the TLS
                // upgrade) and often the addresses are not resolved yet, so the
                // device would be created without a fingerprint. Report only from
                // the resolved/updated callbacks below, which carry the TXT.
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                    nsdManager.registerServiceInfoCallback(
                        service,
                        { it.run() },
                        object : NsdManager.ServiceInfoCallback {
                            override fun onServiceUpdated(serviceInfo: NsdServiceInfo) {
                                Log.v(TAG, "onServiceUpdated: $serviceInfo")
                                val serviceName = serviceInfo.serviceName ?: return
                                addOrUpdate(
                                    serviceName,
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
                    enqueueResolve(PendingResolve(service, addOrUpdate))
                }
            }
        }
    }

    private fun enqueueResolve(pending: PendingResolve) {
        synchronized(resolveQueue) {
            val duplicate = resolveQueue.any {
                it.service.serviceName == pending.service.serviceName &&
                    it.service.serviceType == pending.service.serviceType
            }
            if (duplicate) return
            resolveQueue.addLast(pending)
            if (resolveInFlight) return
            resolveInFlight = true
        }
        resolveNext()
    }

    private fun resolveNext() {
        val pending: PendingResolve
        synchronized(resolveQueue) {
            val next = resolveQueue.removeFirstOrNull()
            if (next == null) {
                resolveInFlight = false
                return
            }
            pending = next
        }
        pending.attempts += 1
        // One-shot completion latch shared by the listener and the watchdog.
        // Some pre-34 NSD stacks drop a resolve callback entirely, which would
        // wedge the serialized queue forever, so the watchdog force-advances.
        // Whoever wins the latch advances the queue, everyone else is a no-op.
        val done = AtomicBoolean(false)
        retryHandler.postDelayed({
            if (done.compareAndSet(false, true)) {
                Log.w(TAG, "Resolve timed out for ${pending.service.serviceName}, advancing queue")
                resolveNext()
            }
        }, RESOLVE_TIMEOUT_MS)
        val listener = object : NsdManager.ResolveListener {
            override fun onResolveFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {
                Log.v(TAG, "Resolve failed for ${serviceInfo.serviceName}: $errorCode")
                if (!done.compareAndSet(false, true)) return
                // ALREADY_ACTIVE means something outside this class holds the
                // resolve slot, retry with backoff. Other codes are not
                // transient, the device gets a chance on its next announcement.
                if (errorCode == NsdManager.FAILURE_ALREADY_ACTIVE &&
                    pending.attempts < MAX_RESOLVE_ATTEMPTS
                ) {
                    val delay = RESOLVE_RETRY_DELAY_MS shl (pending.attempts - 1)
                    retryHandler.postDelayed({
                        synchronized(resolveQueue) { resolveQueue.addFirst(pending) }
                        resolveNext()
                    }, delay)
                } else {
                    resolveNext()
                }
            }

            override fun onServiceResolved(serviceInfo: NsdServiceInfo) {
                Log.v(TAG, "Resolve Succeeded: $serviceInfo")
                val serviceName = serviceInfo.serviceName
                val hostAddr = serviceInfo.host
                if (serviceName != null && hostAddr != null) {
                    // a late result after the watchdog fired is still reported,
                    // only the queue advance is latched
                    pending.addOrUpdate(
                        serviceName,
                        inetAddressToIpAddr(arrayOf(hostAddr)),
                        serviceInfo.port.toUShort(),
                        serviceInfo.attributes,
                        false,
                    )
                }
                if (done.compareAndSet(false, true)) resolveNext()
            }
        }
        try {
            nsdManager.resolveService(pending.service, listener)
        } catch (e: Throwable) {
            Log.w(TAG, "resolveService threw for ${pending.service.serviceName}", e)
            if (done.compareAndSet(false, true)) resolveNext()
        }
    }

    private fun chromecastDeviceEvent(
        name: String,
        addresses: List<IpAddr>,
        port: UShort,
        txt: Map<String, ByteArray?>,
        lost: Boolean,
    ) {
        val fullName = "$name._googlecast._tcp"
        if (lost) {
            devices.remove(fullName)?.let { eventHandler.deviceRemoved(it) }
            return
        }
        val txtRecords = decodeTxtRecords(txt)
        val friendlyName = txtRecords["fn"]
        if (friendlyName.isNullOrEmpty()) {
            Log.d(TAG, "Ignoring Chromecast `$name`: no friendly name in the TXT records")
            return
        }

        val deviceInfo =
            DeviceInfo(friendlyName, ProtocolType.CHROMECAST, addresses, port, txtRecords)
        if (devices.containsKey(fullName)) {
            eventHandler.deviceChanged(deviceInfo)
        } else {
            eventHandler.deviceAvailable(deviceInfo)
            devices[fullName] = friendlyName
        }
    }

    private fun fCastDeviceEvent(
        name: String,
        addresses: List<IpAddr>,
        port: UShort,
        txt: Map<String, ByteArray?>,
        lost: Boolean,
    ) {
        val fullName = "$name._fcast._tcp"
        if (lost) {
            devices.remove(fullName)?.let { eventHandler.deviceRemoved(it) }
            return
        }
        val deviceInfo = DeviceInfo(name, ProtocolType.F_CAST, addresses, port,
            decodeTxtRecords(txt))
        if (devices.containsKey(fullName)) {
            eventHandler.deviceChanged(deviceInfo)
        } else {
            eventHandler.deviceAvailable(deviceInfo)
            devices[fullName] = name
        }
    }

    companion object {
        private val TAG = "NsdDeviceDiscoverer"
        // Exponential backoff, 500ms doubling per attempt covers ~15s of the
        // host app holding the app-wide resolve slot before giving up.
        private const val MAX_RESOLVE_ATTEMPTS = 6
        private const val RESOLVE_RETRY_DELAY_MS = 500L
        private const val RESOLVE_TIMEOUT_MS = 15_000L
    }
}

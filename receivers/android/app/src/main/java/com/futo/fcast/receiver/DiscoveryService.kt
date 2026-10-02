package com.futo.fcast.receiver

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.net.wifi.WifiManager
import android.util.Log
import com.futo.fcast.receiver.models.PROTOCOL_VERSION

class DiscoveryService(private val _context: Context) {
    private var _nsdManager: NsdManager? = null
    private var _registrationListenerTcp: DefaultRegistrationListener? = null
    private var _multicastLock: WifiManager.MulticastLock? = null

    @Synchronized
    fun start() {
        if (_nsdManager != null) return

        val serviceName = getServiceName()
        Log.i(TAG, "Discovery service started. Name: $serviceName")
        val listener = DefaultRegistrationListener { failedListener ->
            synchronized(this) {
                if (_registrationListenerTcp === failedListener) reset()
            }
        }

        try {
            val wifiManager = _context.applicationContext
                .getSystemService(Context.WIFI_SERVICE) as WifiManager
            _multicastLock = wifiManager.createMulticastLock(TAG).apply {
                setReferenceCounted(false)
            }
            _multicastLock!!.acquire()
            _nsdManager = _context.getSystemService(Context.NSD_SERVICE) as NsdManager
            _registrationListenerTcp = listener
            _nsdManager!!.registerService(NsdServiceInfo().apply {
                this.serviceName = serviceName
                this.serviceType = "_fcast._tcp"
                this.port = TcpListenerService.PORT

                this.setAttribute("version", PROTOCOL_VERSION.toString())
                this.setAttribute("appName", BuildConfig.VERSION_NAME)
                this.setAttribute("appVersion", BuildConfig.VERSION_CODE.toString())
            }, NsdManager.PROTOCOL_DNS_SD, listener)
        } catch (e: Exception) {
            reset()
            throw e
        }
    }

    @Synchronized
    fun stop() {
        try {
            _registrationListenerTcp?.let { _nsdManager?.unregisterService(it) }
        } catch (e: Exception) {
            Log.e(TAG, "Failed to unregister TCP Listener.", e)
        } finally {
            reset()
        }
    }

    private fun reset() {
        _registrationListenerTcp = null
        _nsdManager = null
        val multicastLock = _multicastLock
        _multicastLock = null
        if (multicastLock?.isHeld == true) multicastLock.release()
    }

    private class DefaultRegistrationListener(
        private val onFailure: (DefaultRegistrationListener) -> Unit
    ) : NsdManager.RegistrationListener {
        override fun onServiceRegistered(serviceInfo: NsdServiceInfo) {
            Log.d(TAG, "Service registered: ${serviceInfo.serviceName}")
        }

        override fun onRegistrationFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {
            Log.e(TAG, "Service registration failed: serviceInfo=$serviceInfo errorCode=$errorCode")
            onFailure(this)
        }

        override fun onServiceUnregistered(serviceInfo: NsdServiceInfo) {
            Log.d(TAG, "Service unregistered: ${serviceInfo.serviceName}")
        }

        override fun onUnregistrationFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {
            Log.e(TAG, "Service unregistration failed: errorCode=$errorCode")
        }
    }

    companion object {
        private const val TAG = "DiscoveryService"

        fun getServiceName(): String {
            val modelName = if (android.os.Build.MODEL.contains(android.os.Build.MANUFACTURER))
                android.os.Build.MODEL.removePrefix(android.os.Build.MANUFACTURER).trim()
            else android.os.Build.MODEL

            return "FCast-${android.os.Build.MANUFACTURER}-$modelName"
        }
    }
}

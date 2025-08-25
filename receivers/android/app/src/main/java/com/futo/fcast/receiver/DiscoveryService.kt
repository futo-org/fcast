package com.futo.fcast.receiver

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.util.Log
import com.futo.fcast.receiver.models.PROTOCOL_VERSION

class DiscoveryService(private val _context: Context) {
    private var _nsdManager: NsdManager? = null
    private val _registrationListenerTcp = DefaultRegistrationListener()
    private val _registrationListenerWs = DefaultRegistrationListener()

    fun start() {
        if (_nsdManager != null) return

        val serviceName = getServiceName()
        Log.i(TAG, "Discovery service started. Name: $serviceName")

        _nsdManager = _context.getSystemService(Context.NSD_SERVICE) as NsdManager
        _nsdManager?.registerService(NsdServiceInfo().apply {
            this.serviceName = serviceName
            this.serviceType = "_fcast._tcp"
            this.port = TcpListenerService.PORT

            this.setAttribute("version", PROTOCOL_VERSION.toString())
            this.setAttribute("appName", BuildConfig.VERSION_NAME)
            this.setAttribute("appVersion", BuildConfig.VERSION_CODE.toString())
        }, NsdManager.PROTOCOL_DNS_SD, _registrationListenerTcp)

        _nsdManager?.registerService(NsdServiceInfo().apply {
            this.serviceName = serviceName
            this.serviceType = "_fcast._ws"
            this.port = WebSocketListenerService.PORT

            this.setAttribute("version", PROTOCOL_VERSION.toString())
            this.setAttribute("appName", BuildConfig.VERSION_NAME)
            this.setAttribute("appVersion", BuildConfig.VERSION_CODE.toString())
        }, NsdManager.PROTOCOL_DNS_SD, _registrationListenerWs)
    }

    fun stop() {
        if (_nsdManager == null) return

        try {
            _nsdManager?.unregisterService(_registrationListenerTcp)
        } catch (_: Throwable) {
            Log.e(TAG, "Failed to unregister TCP Listener.")
        }

        try {
            _nsdManager?.unregisterService(_registrationListenerWs)
        } catch (_: Throwable) {
            Log.e(TAG, "Failed to unregister TCP Listener.")
        }

        _nsdManager = null
    }

    private class DefaultRegistrationListener : NsdManager.RegistrationListener {
        override fun onServiceRegistered(serviceInfo: NsdServiceInfo) {
            Log.d(TAG, "Service registered: ${serviceInfo.serviceName}")
        }

        override fun onRegistrationFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {
            Log.e(TAG, "Service registration failed: serviceInfo=$serviceInfo errorCode=$errorCode")
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
            return "FCast-${android.os.Build.MANUFACTURER}-${android.os.Build.MODEL}"
        }
    }
}

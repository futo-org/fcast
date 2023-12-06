package com.futo.fcast.receiver

import WebSocketListenerService
import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.util.Log

class DiscoveryService(private val _context: Context) {
    private var _nsdManager: NsdManager? = null
    private val _registrationListenerTcp = DefaultRegistrationListener()
    private val _registrationListenerWs = DefaultRegistrationListener()

    private fun getDeviceName(): String {
        return "${android.os.Build.MANUFACTURER}-${android.os.Build.MODEL}"
    }

    fun start() {
        if (_nsdManager != null) return

        val serviceName = "FCast-${getDeviceName()}"
        Log.i("DiscoveryService", "Discovery service started. Name: $serviceName")

        _nsdManager = _context.getSystemService(Context.NSD_SERVICE) as NsdManager
        _nsdManager?.registerService(NsdServiceInfo().apply {
            this.serviceName = serviceName
            this.serviceType = "_fcast._tcp"
            this.port = TcpListenerService.PORT
        }, NsdManager.PROTOCOL_DNS_SD, _registrationListenerTcp)

        _nsdManager?.registerService(NsdServiceInfo().apply {
            this.serviceName = serviceName
            this.serviceType = "_fcast._ws"
            this.port = WebSocketListenerService.PORT
        }, NsdManager.PROTOCOL_DNS_SD, _registrationListenerWs)
    }

    fun stop() {
        if (_nsdManager == null) return

        _nsdManager?.unregisterService(_registrationListenerTcp)
        _nsdManager?.unregisterService(_registrationListenerWs)
        _nsdManager = null
    }

    private class DefaultRegistrationListener : NsdManager.RegistrationListener {
        override fun onServiceRegistered(serviceInfo: NsdServiceInfo) {
            Log.d("DiscoveryService", "Service registered: ${serviceInfo.serviceName}")
        }

        override fun onRegistrationFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {
            Log.e("DiscoveryService", "Service registration failed: errorCode=$errorCode")
        }

        override fun onServiceUnregistered(serviceInfo: NsdServiceInfo) {
            Log.d("DiscoveryService", "Service unregistered: ${serviceInfo.serviceName}")
        }

        override fun onUnregistrationFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {
            Log.e("DiscoveryService", "Service unregistration failed: errorCode=$errorCode")
        }
    }
}
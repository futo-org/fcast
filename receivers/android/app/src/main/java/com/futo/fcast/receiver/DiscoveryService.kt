package com.futo.fcast.receiver

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.util.Log

class DiscoveryService(private val _context: Context) {
    private var _nsdManager: NsdManager? = null
    private val _serviceType = "_fcast._tcp"

    private fun getDeviceName(): String {
        return "${android.os.Build.MANUFACTURER}-${android.os.Build.MODEL}"
    }

    fun start() {
        if (_nsdManager != null) return

        val serviceName = "FCast-${getDeviceName()}"
        Log.i("DiscoveryService", "Discovery service started. Name: $serviceName")

        _nsdManager = _context.getSystemService(Context.NSD_SERVICE) as NsdManager
        val serviceInfo = NsdServiceInfo().apply {
            this.serviceName = serviceName
            this.serviceType = _serviceType
            this.port = 46899
        }

        _nsdManager?.registerService(serviceInfo, NsdManager.PROTOCOL_DNS_SD, registrationListener)
    }

    fun stop() {
        if (_nsdManager == null) return

        _nsdManager?.unregisterService(registrationListener)
        _nsdManager = null
    }

    private val registrationListener = object : NsdManager.RegistrationListener {
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
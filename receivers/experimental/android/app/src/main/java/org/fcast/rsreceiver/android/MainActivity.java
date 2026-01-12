package org.fcast.rsreceiver.android;

import android.annotation.SuppressLint;
import android.content.Context;
import android.net.ConnectivityManager;
import android.net.LinkAddress;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.NetworkRequest;
import android.net.nsd.NsdManager;
import android.net.nsd.NsdServiceInfo;
import android.net.wifi.WifiManager;
import android.os.Build;
import android.os.Bundle;
import android.app.NativeActivity;
import android.os.PowerManager;
import android.util.Log;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.WindowManager;

import androidx.annotation.NonNull;

import org.freedesktop.gstreamer.GStreamer;

import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.List;

public class MainActivity extends NativeActivity implements NsdManager.RegistrationListener {
    NsdManager nsdManager = null;
    WifiManager wifiManager = null;
    WifiManager.WifiLock wifiLock = null;
    PowerManager powerManager = null;
    PowerManager.WakeLock cpuWakeLock = null;
    ConnectivityManager connectivityManager = null;

    void networkEvent(boolean available, @NonNull Network network) {
        if (connectivityManager == null) {
            return;
        }

        LinkProperties props = connectivityManager.getLinkProperties(network);
        if (props != null) {
            // props.getLinkAddresses().stream().map(addrConvert);

            ArrayList<ByteBuffer> addrs = new ArrayList();

            for (LinkAddress linkAddr: props.getLinkAddresses()) {
                byte[] addressBytes = linkAddr.getAddress().getAddress();
                ByteBuffer buf = ByteBuffer.allocateDirect(addressBytes.length);
                buf.put(addressBytes);
                addrs.add(buf);
            }

            Log.d("networkEvent", "available=" + available + " addrs=" + addrs);
            nativeNetworkEvent(available, addrs);
        }
    }

    native void nativeNetworkEvent(boolean available, List<ByteBuffer> addrs);

    class NetworkCallbackHandler extends ConnectivityManager.NetworkCallback {
        @Override
        public void onAvailable(@NonNull Network network) {
            networkEvent(true, network);
        }

        @Override
        public void onLost(@NonNull Network network) {
            networkEvent(true, network);
        }
    }

    static {
        System.loadLibrary("gstreamer_android");
        System.loadLibrary("fcastreceiver");
    }

    @SuppressLint("WakelockTimeout")
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        try {
            GStreamer.init(this);
        } catch (Exception e) {
            Log.e("MAIN_ACTIVITY", "Failed to init GStreamer ${e}");
            finish();
        }

        nsdManager = (NsdManager) this.getSystemService(Context.NSD_SERVICE);
        NsdServiceInfo serviceInfo = new NsdServiceInfo();
        serviceInfo.setServiceName("FCast-TODO");
        serviceInfo.setServiceType("_fcast._tcp");
        serviceInfo.setPort(46899);
        nsdManager.registerService(serviceInfo, NsdManager.PROTOCOL_DNS_SD, this);

        connectivityManager = (ConnectivityManager) this.getSystemService(Context.CONNECTIVITY_SERVICE);
        NetworkRequest networkRequest = new NetworkRequest.Builder()
                .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
                .addTransportType(NetworkCapabilities.TRANSPORT_ETHERNET)
                .build();
        connectivityManager.registerNetworkCallback(networkRequest, new NetworkCallbackHandler());

        wifiManager = (WifiManager) this.getSystemService(Context.WIFI_SERVICE);
        wifiLock = wifiManager.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "FCastRsReceiver:WifiLock");
        wifiLock.acquire();

        powerManager = (PowerManager) this.getSystemService(Context.POWER_SERVICE);
        cpuWakeLock = powerManager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "FCastRsReceiver:WakeLock");
        cpuWakeLock.acquire();
    }

    @SuppressLint("WakelockTimeout")
    @Override
    protected void onResume() {
        super.onResume();
        cpuWakeLock.acquire();
    }

    @Override
    protected void onPause() {
        super.onPause();
        cpuWakeLock.release();
    }

    @Override
    protected void onDestroy() {
        super.onDestroy();
        nsdManager.unregisterService(this);
        wifiLock.release();
    }

    @Override
    public void onRegistrationFailed(NsdServiceInfo serviceInfo, int errorCode) { }

    @Override
    public void onUnregistrationFailed(NsdServiceInfo serviceInfo, int errorCode) { }

    @Override
    public void onServiceRegistered(NsdServiceInfo serviceInfo) { }

    @Override
    public void onServiceUnregistered(NsdServiceInfo serviceInfo) { }
}

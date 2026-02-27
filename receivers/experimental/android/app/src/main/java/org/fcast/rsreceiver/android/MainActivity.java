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
import android.os.Bundle;
import android.app.NativeActivity;
import android.os.PowerManager;
import android.util.Log;
import androidx.annotation.NonNull;
import org.freedesktop.gstreamer.GStreamer;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

class DummyNsdRegistrationListener implements NsdManager.RegistrationListener {

    @Override
    public void onRegistrationFailed(NsdServiceInfo serviceInfo, int errorCode) { }

    @Override
    public void onUnregistrationFailed(NsdServiceInfo serviceInfo, int errorCode) { }

    @Override
    public void onServiceRegistered(NsdServiceInfo serviceInfo) { }

    @Override
    public void onServiceUnregistered(NsdServiceInfo serviceInfo) { }
}

public class MainActivity extends NativeActivity {
    NsdManager nsdManager = null;
    WifiManager wifiManager = null;
    WifiManager.WifiLock wifiLock = null;
    PowerManager powerManager = null;
    PowerManager.WakeLock cpuWakeLock = null;
    ConnectivityManager connectivityManager = null;
    DummyNsdRegistrationListener fcastNsdReg = new DummyNsdRegistrationListener();
    DummyNsdRegistrationListener raopNsdReg = new DummyNsdRegistrationListener();

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
    native void setMdnsDeviceName(String name);
    native String getDeviceNameRaopHash(String name);
    native void getRaopTxtAttribs(Map<String, String> attrs);

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
        NsdServiceInfo fcastServiceInfo = new NsdServiceInfo();
        NsdServiceInfo raopServiceInfo = new NsdServiceInfo();

        String modelName;
        if (android.os.Build.MODEL.contains(android.os.Build.MANUFACTURER)) {
            modelName = android.os.Build.MODEL.replaceFirst("^" + android.os.Build.MANUFACTURER, "").trim();
        } else {
            modelName = android.os.Build.MODEL;
        }
        String serviceName = "FCast-" + android.os.Build.MANUFACTURER + "-" + modelName;

        setMdnsDeviceName(serviceName);
        fcastServiceInfo.setServiceName(serviceName);
        fcastServiceInfo.setServiceType("_fcast._tcp");
        fcastServiceInfo.setPort(46899);
        nsdManager.registerService(fcastServiceInfo, NsdManager.PROTOCOL_DNS_SD, fcastNsdReg);

        String raopHash = getDeviceNameRaopHash(serviceName);
        raopServiceInfo.setServiceName(raopHash + "@" + serviceName);
        raopServiceInfo.setServiceType("_raop._tcp");
        raopServiceInfo.setPort(33505);
        Map<String, String> raopAttrs = new HashMap<>();
        getRaopTxtAttribs(raopAttrs);
        for (Map.Entry<String, String> a : raopAttrs.entrySet()) {
            raopServiceInfo.setAttribute(a.getKey(), a.getValue());
        }
        nsdManager.registerService(raopServiceInfo, NsdManager.PROTOCOL_DNS_SD, raopNsdReg);

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
        nsdManager.unregisterService(fcastNsdReg);
        nsdManager.unregisterService(raopNsdReg);
        wifiLock.release();
    }
}

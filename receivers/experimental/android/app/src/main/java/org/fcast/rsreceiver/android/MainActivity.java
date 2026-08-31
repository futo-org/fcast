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
    private boolean fcastNsdRegistered = false;
    private final android.os.Handler fcastNsdHandler =
            new android.os.Handler(android.os.Looper.getMainLooper());
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
    native boolean getFCastTxtAttribs(Map<String, String> attrs);

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
        // One self-contained library: GStreamer is statically linked inside
        // and initialized by the native side, no java glue involved.
        System.loadLibrary("fcastreceiver");
    }

    @SuppressLint("WakelockTimeout")
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        // Translucent from the first frame. The video hole punch needs it
        // anyway, and switching later recreates the window surface mid
        // launch, which flashes the launcher through an empty window.
        getWindow().setFormat(android.graphics.PixelFormat.TRANSLUCENT);
        // the theme's windowBackground is only for the starting window, on
        // the live window it would paint over the video hole punch
        getWindow().setBackgroundDrawable(null);

        // A cast receiver is a full-bleed surface: immersive sticky, video
        // may extend into a display cutout, the chrome pads by the reported
        // safe area.
        if (android.os.Build.VERSION.SDK_INT >= 28) {
            android.view.WindowManager.LayoutParams lp = getWindow().getAttributes();
            lp.layoutInDisplayCutoutMode =
                    android.view.WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES;
            getWindow().setAttributes(lp);
        }
        // Not immersive at start: the player's fullscreen toggle drives
        // it. Insets flow through slint's own android backend into
        // Window.safe-area-insets, no bridge needed.

        nsdManager = (NsdManager) this.getSystemService(Context.NSD_SERVICE);
        NsdServiceInfo raopServiceInfo = new NsdServiceInfo();

        String modelName;
        if (android.os.Build.MODEL.contains(android.os.Build.MANUFACTURER)) {
            modelName = android.os.Build.MODEL.replaceFirst("^" + android.os.Build.MANUFACTURER, "").trim();
        } else {
            modelName = android.os.Build.MODEL;
        }
        String serviceName = "FCast-" + android.os.Build.MANUFACTURER + "-" + modelName;

        setMdnsDeviceName(serviceName);
        // Registration waits for the TXT records (TLS fingerprint + protocol
        // version): v4 senders key their secure connect on them, so an
        // advertisement without them is worse than a briefly delayed one.
        registerFCastWhenReady(serviceName);

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

    private void registerFCastWhenReady(String serviceName) {
        Map<String, String> attrs = new HashMap<>();
        if (!getFCastTxtAttribs(attrs)) {
            fcastNsdHandler.postDelayed(() -> registerFCastWhenReady(serviceName), 200);
            return;
        }
        NsdServiceInfo info = new NsdServiceInfo();
        info.setServiceName(serviceName);
        info.setServiceType("_fcast._tcp");
        info.setPort(46899);
        for (Map.Entry<String, String> a : attrs.entrySet()) {
            info.setAttribute(a.getKey(), a.getValue());
        }
        nsdManager.registerService(info, NsdManager.PROTOCOL_DNS_SD, fcastNsdReg);
        fcastNsdRegistered = true;
    }

    /// Called from native code around playback. Any thread. Replaces
    /// android-activity's set_window_flags, whose process-wide RwLock
    /// deadlocks against the slint event loop's long-held read guard.
    public void setKeepScreenOn(boolean on) {
        runOnUiThread(() -> {
            if (on) {
                getWindow().addFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            } else {
                getWindow().clearFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            }
        });
    }

    private boolean immersiveWanted = false;

    /// Called from native code (the player's fullscreen toggle). Any thread.
    public void setImmersive(boolean on) {
        immersiveWanted = on;
        runOnUiThread(() -> {
            if (on) {
                enterImmersive();
            } else {
                getWindow().getDecorView().setSystemUiVisibility(0);
            }
        });
    }

    private void enterImmersive() {
        getWindow().getDecorView().setSystemUiVisibility(
                android.view.View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                        | android.view.View.SYSTEM_UI_FLAG_FULLSCREEN
                        | android.view.View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                        | android.view.View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                        | android.view.View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                        | android.view.View.SYSTEM_UI_FLAG_LAYOUT_STABLE);
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        // sticky immersive drops on some focus transitions, re-assert
        if (hasFocus && immersiveWanted) {
            enterImmersive();
        }
    }

    /// The video SurfaceView's surface dies with the activity; the native
    /// side detaches it from the player before that and re-adopts a fresh
    /// one on return, otherwise a running codec errors out mid-video.
    native void nativeAppVisibility(boolean visible);

    @Override
    protected void onStart() {
        super.onStart();
        nativeAppVisibility(true);
    }

    @Override
    protected void onStop() {
        nativeAppVisibility(false);
        super.onStop();
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
        fcastNsdHandler.removeCallbacksAndMessages(null);
        if (fcastNsdRegistered) {
            nsdManager.unregisterService(fcastNsdReg);
        }
        nsdManager.unregisterService(raopNsdReg);
        wifiLock.release();
    }
}

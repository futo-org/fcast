package org.fcast.rsreceiver.android;

import android.annotation.SuppressLint;
import android.content.Context;
import android.content.Intent;
import android.media.AudioManager;
import android.media.session.MediaSession;
import android.net.ConnectivityManager;
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
import java.net.InetAddress;
import java.net.NetworkInterface;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class MainActivity extends NativeActivity {
    private static final String TAG = "FCastMainActivity";

    NsdManager nsdManager = null;
    WifiManager wifiManager = null;
    WifiManager.WifiLock wifiLock = null;
    PowerManager powerManager = null;
    PowerManager.WakeLock cpuWakeLock = null;
    ConnectivityManager connectivityManager = null;
    ConnectivityManager.NetworkCallback networkCallback = null;

    private final android.os.Handler handler =
            new android.os.Handler(android.os.Looper.getMainLooper());

    private String serviceName;
    // The listener instance IS the registration handle: one fresh instance per
    // register call, never reused, so re-registration cycles cannot trip
    // NsdManager's listener-in-use checks.
    private NsdListener fcastReg = null;
    private NsdListener raopReg = null;
    private boolean destroyed = false;

    // Re-sweep even without a callback: tethering/hotspot interfaces never
    // surface as Networks, so their addresses are only found by polling.
    private static final long SWEEP_INTERVAL_MS = 30_000;
    private static final long NETWORK_SETTLE_MS = 500;

    /// Registration outcomes matter: a swallowed failure means the receiver
    /// shows its QR while nobody can discover it, and a collision rename
    /// means the UI shows a name the network does not have.
    class NsdListener implements NsdManager.RegistrationListener {
        final String label;
        final boolean isFcast;

        NsdListener(String label, boolean isFcast) {
            this.label = label;
            this.isFcast = isFcast;
        }

        @Override
        public void onRegistrationFailed(NsdServiceInfo info, int errorCode) {
            Log.e(TAG, label + " registration failed: " + errorCode + ", retrying");
            handler.postDelayed(() -> {
                if (!destroyed) {
                    registerServices();
                }
            }, 3_000);
        }

        @Override
        public void onUnregistrationFailed(NsdServiceInfo info, int errorCode) {
            Log.e(TAG, label + " unregistration failed: " + errorCode);
        }

        @Override
        public void onServiceRegistered(NsdServiceInfo info) {
            Log.i(TAG, label + " registered as " + info.getServiceName());
            // The daemon renames on collision. Adopt the effective name so
            // the UI and the network agree.
            if (isFcast && !info.getServiceName().equals(serviceName)) {
                serviceName = info.getServiceName();
                setMdnsDeviceName(serviceName);
            }
        }

        @Override
        public void onServiceUnregistered(NsdServiceInfo info) { }
    }

    native void nativeSetAddresses(List<ByteBuffer> addrs);
    native void setMdnsDeviceName(String name);
    native String getDeviceNameRaopHash(String name);
    native void getRaopTxtAttribs(Map<String, String> attrs);
    native boolean getFCastTxtAttribs(Map<String, String> attrs);
    native int getFCastPort();

    /// One authoritative sweep instead of per-Network bookkeeping: every
    /// interface's current addresses, as the full replacement set. Covers
    /// hotspot/tethering interfaces the ConnectivityManager never reports.
    void sweepAddresses() {
        ArrayList<ByteBuffer> addrs = new ArrayList<>();
        try {
            Enumeration<NetworkInterface> ifaces = NetworkInterface.getNetworkInterfaces();
            while (ifaces != null && ifaces.hasMoreElements()) {
                NetworkInterface iface = ifaces.nextElement();
                if (!iface.isUp() || iface.isLoopback()) {
                    continue;
                }
                Enumeration<InetAddress> ifaceAddrs = iface.getInetAddresses();
                while (ifaceAddrs.hasMoreElements()) {
                    InetAddress addr = ifaceAddrs.nextElement();
                    if (addr.isLoopbackAddress()) {
                        continue;
                    }
                    byte[] addressBytes = addr.getAddress();
                    ByteBuffer buf = ByteBuffer.allocateDirect(addressBytes.length);
                    buf.put(addressBytes);
                    addrs.add(buf);
                }
            }
        } catch (java.net.SocketException e) {
            Log.e(TAG, "interface sweep failed", e);
            return;
        }
        Log.d(TAG, "address sweep: " + addrs.size() + " addresses");
        nativeSetAddresses(addrs);
    }

    /// Debounced reaction to any connectivity signal: re-sweep addresses and
    /// re-register NSD, since the platform responder's registrations go
    /// stale across interface changes.
    private final Runnable networkSettled = () -> {
        if (destroyed) {
            return;
        }
        sweepAddresses();
        registerServices();
    };

    void onNetworkChanged() {
        handler.removeCallbacks(networkSettled);
        handler.postDelayed(networkSettled, NETWORK_SETTLE_MS);
    }

    private final Runnable periodicSweep = new Runnable() {
        @Override
        public void run() {
            if (destroyed) {
                return;
            }
            sweepAddresses();
            handler.postDelayed(this, SWEEP_INTERVAL_MS);
        }
    };

    class NetworkCallbackHandler extends ConnectivityManager.NetworkCallback {
        @Override
        public void onAvailable(@NonNull Network network) {
            onNetworkChanged();
        }

        @Override
        public void onLost(@NonNull Network network) {
            onNetworkChanged();
        }

        @Override
        public void onLinkPropertiesChanged(@NonNull Network network,
                @NonNull LinkProperties props) {
            // AP roams, DHCP renews and IPv6 prefix changes keep the same
            // Network and only fire this.
            onNetworkChanged();
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

        // Hardware volume keys drive the media stream; without this they hit
        // the ring stream whenever nothing is actively playing.
        setVolumeControlStream(AudioManager.STREAM_MUSIC);

        createMediaSession();
        ReceiverService.ensureChannel(this);
        if (android.os.Build.VERSION.SDK_INT >= 33
                && checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS)
                        != android.content.pm.PackageManager.PERMISSION_GRANTED) {
            requestPermissions(
                    new String[] { android.Manifest.permission.POST_NOTIFICATIONS }, 1);
        }

        // A cast receiver is a full-bleed surface: immersive sticky, video
        // may extend into a display cutout, the chrome pads by the reported
        // safe area.
        if (android.os.Build.VERSION.SDK_INT >= 28) {
            android.view.WindowManager.LayoutParams lp = getWindow().getAttributes();
            // ALWAYS on 30+: SHORT_EDGES letterboxes away from a long-edge
            // notch in landscape, which is where video wants the pixels.
            // The safe-area insets keep the chrome clear of it either way.
            lp.layoutInDisplayCutoutMode = android.os.Build.VERSION.SDK_INT >= 30
                    ? android.view.WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
                    : android.view.WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES;
            getWindow().setAttributes(lp);
        }
        // Not immersive at start: the player's fullscreen toggle drives
        // it. Insets flow through slint's own android backend into
        // Window.safe-area-insets, no bridge needed.

        nsdManager = (NsdManager) this.getSystemService(Context.NSD_SERVICE);

        String modelName;
        if (android.os.Build.MODEL.contains(android.os.Build.MANUFACTURER)) {
            modelName = android.os.Build.MODEL.replaceFirst("^" + android.os.Build.MANUFACTURER, "").trim();
        } else {
            modelName = android.os.Build.MODEL;
        }
        serviceName = "FCast-" + android.os.Build.MANUFACTURER + "-" + modelName;

        setMdnsDeviceName(serviceName);
        registerServices();

        connectivityManager = (ConnectivityManager) this.getSystemService(Context.CONNECTIVITY_SERVICE);
        NetworkRequest networkRequest = new NetworkRequest.Builder()
                .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
                .addTransportType(NetworkCapabilities.TRANSPORT_ETHERNET)
                .build();
        networkCallback = new NetworkCallbackHandler();
        connectivityManager.registerNetworkCallback(networkRequest, networkCallback);

        sweepAddresses();
        handler.postDelayed(periodicSweep, SWEEP_INTERVAL_MS);

        // Created here, acquired only while playback is active, see
        // setPlaybackActive. Non ref-counted so repeated acquires are
        // idempotent and one release always drops the lock.
        wifiManager = (WifiManager) getApplicationContext().getSystemService(Context.WIFI_SERVICE);
        int wifiMode = android.os.Build.VERSION.SDK_INT >= 29
                ? WifiManager.WIFI_MODE_FULL_LOW_LATENCY
                : WifiManager.WIFI_MODE_FULL_HIGH_PERF;
        wifiLock = wifiManager.createWifiLock(wifiMode, "FCastRsReceiver:WifiLock");
        wifiLock.setReferenceCounted(false);

        powerManager = (PowerManager) this.getSystemService(Context.POWER_SERVICE);
        cpuWakeLock = powerManager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "FCastRsReceiver:WakeLock");
        cpuWakeLock.setReferenceCounted(false);
    }

    /// (Re-)register both services, dropping any prior registrations first.
    /// The fcast one waits for the TXT records (TLS fingerprint + protocol
    /// version) AND the committed listen port: v4 senders key their secure
    /// connect on the records, and an advertisement pointing at an unbound
    /// port hands early senders a connection refuse.
    private void registerServices() {
        if (fcastReg != null) {
            nsdManager.unregisterService(fcastReg);
            fcastReg = null;
        }
        if (raopReg != null) {
            nsdManager.unregisterService(raopReg);
            raopReg = null;
        }

        NsdServiceInfo raopServiceInfo = new NsdServiceInfo();
        String raopHash = getDeviceNameRaopHash(serviceName);
        raopServiceInfo.setServiceName(raopHash + "@" + serviceName);
        raopServiceInfo.setServiceType("_raop._tcp");
        raopServiceInfo.setPort(33505);
        Map<String, String> raopAttrs = new HashMap<>();
        getRaopTxtAttribs(raopAttrs);
        for (Map.Entry<String, String> a : raopAttrs.entrySet()) {
            raopServiceInfo.setAttribute(a.getKey(), a.getValue());
        }
        raopReg = new NsdListener("_raop", false);
        nsdManager.registerService(raopServiceInfo, NsdManager.PROTOCOL_DNS_SD, raopReg);

        registerFCastWhenReady();
    }

    private void registerFCastWhenReady() {
        Map<String, String> attrs = new HashMap<>();
        int port = getFCastPort();
        if (port == 0 || !getFCastTxtAttribs(attrs)) {
            handler.postDelayed(() -> {
                if (!destroyed) {
                    registerFCastWhenReady();
                }
            }, 200);
            return;
        }
        NsdServiceInfo info = new NsdServiceInfo();
        info.setServiceName(serviceName);
        info.setServiceType("_fcast._tcp");
        info.setPort(port);
        for (Map.Entry<String, String> a : attrs.entrySet()) {
            info.setAttribute(a.getKey(), a.getValue());
        }
        fcastReg = new NsdListener("_fcast", true);
        nsdManager.registerService(info, NsdManager.PROTOCOL_DNS_SD, fcastReg);
    }

    private android.media.AudioFocusRequest focusRequest = null;
    private android.content.BroadcastReceiver noisyReceiver = null;

    private final AudioManager.OnAudioFocusChangeListener focusListener = change -> {
        switch (change) {
            case AudioManager.AUDIOFOCUS_LOSS:
                nativeAudioEvent(0);
                break;
            case AudioManager.AUDIOFOCUS_LOSS_TRANSIENT:
            case AudioManager.AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK:
                nativeAudioEvent(1);
                break;
            case AudioManager.AUDIOFOCUS_GAIN:
                nativeAudioEvent(2);
                break;
        }
    };

    /// Codes: 0 loss, 1 transient loss, 2 gain, 3 becoming noisy. The pause
    /// and resume policy lives in native code, which knows the player state.
    native void nativeAudioEvent(int code);

    /// Transport commands from the MediaSession and the notification:
    /// 0 stop, 1 pause, 2 resume. Static so ReceiverService can send too.
    static native void nativeMediaCommand(int code);

    /// Absolute seek from the session (lock screen, BT remote), seconds.
    static native void nativeMediaSeek(double seconds);

    private MediaSession mediaSession = null;

    /// The session: what routes media buttons, drives the lock-screen and
    /// BT transport surfaces, and feeds the notification's MediaStyle.
    private void createMediaSession() {
        mediaSession = new MediaSession(this, "FCastReceiver");
        mediaSession.setCallback(new MediaSession.Callback() {
            @Override
            public void onPlay() {
                nativeMediaCommand(2);
            }

            @Override
            public void onPause() {
                nativeMediaCommand(1);
            }

            @Override
            public void onStop() {
                nativeMediaCommand(0);
            }

            @Override
            public void onSeekTo(long posMs) {
                nativeMediaSeek(posMs / 1000.0);
            }
        });
        mediaSession.setPlaybackToLocal(new android.media.AudioAttributes.Builder()
                .setUsage(android.media.AudioAttributes.USAGE_MEDIA)
                .setContentType(android.media.AudioAttributes.CONTENT_TYPE_MOVIE)
                .build());
        ReceiverService.sessionToken = mediaSession.getSessionToken();
    }

    /// Called from native code on every progress tick and state change.
    /// Any thread; MediaSession is thread-safe.
    public void updateMediaSession(boolean playing, long positionMs, float speed) {
        if (mediaSession == null) {
            return;
        }
        android.media.session.PlaybackState.Builder b =
                new android.media.session.PlaybackState.Builder()
                        .setActions(android.media.session.PlaybackState.ACTION_PLAY
                                | android.media.session.PlaybackState.ACTION_PAUSE
                                | android.media.session.PlaybackState.ACTION_PLAY_PAUSE
                                | android.media.session.PlaybackState.ACTION_STOP
                                | android.media.session.PlaybackState.ACTION_SEEK_TO)
                        .setState(playing
                                        ? android.media.session.PlaybackState.STATE_PLAYING
                                        : android.media.session.PlaybackState.STATE_PAUSED,
                                positionMs, speed);
        mediaSession.setPlaybackState(b.build());
    }

    /// Refresh-rate matching: prefer the lowest display mode at the current
    /// resolution whose rate is a near-integer multiple of the content fps
    /// (23.976 picks 24 or 120, never 60). 0 restores no-preference. Called
    /// from native code when a video stream starts and at idle.
    public void setContentFrameRate(float fps) {
        runOnUiThread(() -> {
            android.view.WindowManager.LayoutParams lp = getWindow().getAttributes();
            int modeId = 0;
            if (fps > 0) {
                android.view.Display display = getWindowManager().getDefaultDisplay();
                android.view.Display.Mode current = display.getMode();
                float best = Float.MAX_VALUE;
                for (android.view.Display.Mode mode : display.getSupportedModes()) {
                    if (mode.getPhysicalWidth() != current.getPhysicalWidth()
                            || mode.getPhysicalHeight() != current.getPhysicalHeight()) {
                        continue;
                    }
                    float rate = mode.getRefreshRate();
                    int multiple = Math.round(rate / fps);
                    if (multiple < 1) {
                        continue;
                    }
                    if (Math.abs(rate / fps - multiple) <= 0.02f * multiple && rate < best) {
                        best = rate;
                        modeId = mode.getModeId();
                    }
                }
            }
            if (lp.preferredDisplayModeId != modeId) {
                Log.i(TAG, "preferred display mode " + modeId + " for " + fps + " fps");
                lp.preferredDisplayModeId = modeId;
                getWindow().setAttributes(lp);
            }
        });
    }

    /// Called from native code when the item's title or duration changes.
    public void updateMediaMetadata(String title, long durationMs) {
        if (mediaSession != null) {
            mediaSession.setMetadata(new android.media.MediaMetadata.Builder()
                    .putString(android.media.MediaMetadata.METADATA_KEY_TITLE, title)
                    .putLong(android.media.MediaMetadata.METADATA_KEY_DURATION, durationMs)
                    .build());
        }
        ReceiverService.castTitle = title;
        ReceiverService.refreshIfRunning();
    }

    /// The single owner of every playback-scoped device resource, called
    /// from native code on the playback active/idle edge. Any thread.
    /// Ordering is preserved by posting to the main looper.
    ///
    /// Direct window-flag manipulation rather than android-activity's
    /// set_window_flags, whose process-wide RwLock deadlocks against the
    /// slint event loop's long-held read guard.
    @SuppressLint("WakelockTimeout")
    public void setPlaybackActive(boolean active, boolean visual) {
        runOnUiThread(() -> {
            // The screen only pins for content someone is looking at; an
            // audio cast relies on the wake lock instead.
            if (active && visual) {
                getWindow().addFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            } else {
                getWindow().clearFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            }

            if (mediaSession != null) {
                mediaSession.setActive(active);
            }
            ReceiverService.castActive = active;
            ReceiverService.refreshIfRunning();

            AudioManager am = (AudioManager) getSystemService(Context.AUDIO_SERVICE);
            if (active) {
                cpuWakeLock.acquire();
                wifiLock.acquire();
                if (focusRequest == null) {
                    focusRequest = new android.media.AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
                            .setAudioAttributes(new android.media.AudioAttributes.Builder()
                                    .setUsage(android.media.AudioAttributes.USAGE_MEDIA)
                                    .setContentType(android.media.AudioAttributes.CONTENT_TYPE_MOVIE)
                                    .build())
                            .setOnAudioFocusChangeListener(focusListener)
                            .build();
                    am.requestAudioFocus(focusRequest);
                }
                if (noisyReceiver == null) {
                    noisyReceiver = new android.content.BroadcastReceiver() {
                        @Override
                        public void onReceive(Context context, android.content.Intent intent) {
                            nativeAudioEvent(3);
                        }
                    };
                    registerReceiver(noisyReceiver,
                            new android.content.IntentFilter(AudioManager.ACTION_AUDIO_BECOMING_NOISY));
                }
            } else {
                // release() on an unheld lock throws, and the idle edge can
                // fire without a preceding active one (teardown).
                if (cpuWakeLock.isHeld()) {
                    cpuWakeLock.release();
                }
                if (wifiLock.isHeld()) {
                    wifiLock.release();
                }
                if (focusRequest != null) {
                    am.abandonAudioFocusRequest(focusRequest);
                    focusRequest = null;
                }
                if (noisyReceiver != null) {
                    unregisterReceiver(noisyReceiver);
                    noisyReceiver = null;
                }
            }
        });
    }

    private boolean immersiveWanted = false;

    /// Called from native code (the player's fullscreen toggle). Any thread.
    /// Not named setImmersive: that would shadow Activity.setImmersive.
    public void setImmersiveUi(boolean on) {
        immersiveWanted = on;
        runOnUiThread(() -> {
            if (on) {
                enterImmersive();
            } else {
                exitImmersive();
            }
        });
    }

    /// WindowInsetsController on 30+: the setSystemUiVisibility flags are
    /// disabled by edge-to-edge enforcement at targetSdk 35+. The legacy
    /// path stays for 28/29. Slint consumes the insets either way through
    /// its android backend, so the chrome pads itself.
    private void enterImmersive() {
        if (android.os.Build.VERSION.SDK_INT >= 30) {
            getWindow().setDecorFitsSystemWindows(false);
            android.view.WindowInsetsController c = getWindow().getInsetsController();
            if (c != null) {
                c.setSystemBarsBehavior(
                        android.view.WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
                c.hide(android.view.WindowInsets.Type.systemBars());
            }
        } else {
            getWindow().getDecorView().setSystemUiVisibility(
                    android.view.View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                            | android.view.View.SYSTEM_UI_FLAG_FULLSCREEN
                            | android.view.View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                            | android.view.View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                            | android.view.View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                            | android.view.View.SYSTEM_UI_FLAG_LAYOUT_STABLE);
        }
    }

    private void exitImmersive() {
        if (android.os.Build.VERSION.SDK_INT >= 30) {
            android.view.WindowInsetsController c = getWindow().getInsetsController();
            if (c != null) {
                c.show(android.view.WindowInsets.Type.systemBars());
            }
            getWindow().setDecorFitsSystemWindows(true);
        } else {
            getWindow().getDecorView().setSystemUiVisibility(0);
        }
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
        // The activity is back; its own lifecycle keeps the process warm.
        stopService(new Intent(this, ReceiverService.class));
        nativeAppVisibility(true);
    }

    @Override
    protected void onStop() {
        nativeAppVisibility(false);
        // Backgrounded: without foreground priority the process is a cached
        // kill candidate and the NSD registration dies with it. Started
        // here, inside the background-start grace window.
        if (!destroyed && !isFinishing()) {
            startForegroundService(new Intent(this, ReceiverService.class));
        }
        super.onStop();
    }

    @Override
    protected void onDestroy() {
        // Before super: NativeActivity's onDestroy blocks on the native
        // thread, which exits the process, so anything after it never runs.
        destroyed = true;
        handler.removeCallbacksAndMessages(null);
        if (fcastReg != null) {
            nsdManager.unregisterService(fcastReg);
            fcastReg = null;
        }
        if (raopReg != null) {
            nsdManager.unregisterService(raopReg);
            raopReg = null;
        }
        if (networkCallback != null) {
            connectivityManager.unregisterNetworkCallback(networkCallback);
            networkCallback = null;
        }
        setPlaybackActive(false, false);
        stopService(new Intent(this, ReceiverService.class));
        if (mediaSession != null) {
            mediaSession.release();
            mediaSession = null;
            ReceiverService.sessionToken = null;
        }
        super.onDestroy();
    }
}

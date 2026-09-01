package org.fcast.rsreceiver.android;

import android.annotation.SuppressLint;
import android.app.PendingIntent;
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
import android.os.HandlerThread;
import android.os.PowerManager;
import android.util.Log;
import androidx.annotation.NonNull;
import java.net.InetAddress;
import java.net.NetworkInterface;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.regex.Pattern;

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
    // Interface sweeps walk /proc/net and issue an ioctl per interface,
    // tens of ms on some devices: off the main looper.
    private HandlerThread netThread = null;
    private android.os.Handler netHandler = null;

    /// The name registrations always use. Never reassigned: adopting a
    /// collision-renamed value as the new base compounds " (2)" suffixes on
    /// every re-registration cycle.
    private String baseServiceName;
    // The listener instance IS the registration handle: one fresh instance per
    // register call, never reused, so re-registration cycles cannot trip
    // NsdManager's listener-in-use checks.
    private NsdListener fcastReg = null;
    private NsdListener raopReg = null;
    private boolean destroyed = false;
    /// Cancels stale registerFCastWhenReady poll chains: each
    /// registerServices() bumps it and in-flight lambdas holding an older
    /// value stop, so two overlapping chains cannot both register.
    private int fcastPollGen = 0;
    /// Per-service backoff and single-retry-pending flags: a failure retries
    /// only ITS OWN service (re-registering the healthy one would flap it),
    /// and only one retry is ever queued per service, or two both-fail
    /// rounds would fan out exponentially.
    private int fcastRetries = 0;
    private int raopRetries = 0;
    private boolean fcastRetryPending = false;
    private boolean raopRetryPending = false;

    // Re-sweep even without a callback: tethering/hotspot interfaces never
    // surface as Networks, so their addresses are only found by polling.
    private static final long SWEEP_INTERVAL_MS = 30_000;
    private static final long NETWORK_SETTLE_MS = 500;

    /// NsdManager removes a listener BEFORE delivering onRegistrationFailed,
    /// so unregistering it afterwards throws; every outcome is handled on
    /// the main handler because the callbacks arrive on the connectivity
    /// thread.
    class NsdListener implements NsdManager.RegistrationListener {
        final String label;
        final boolean isFcast;

        NsdListener(String label, boolean isFcast) {
            this.label = label;
            this.isFcast = isFcast;
        }

        @Override
        public void onRegistrationFailed(NsdServiceInfo info, int errorCode) {
            handler.post(() -> {
                // the platform already dropped this listener
                if (isFcast) {
                    if (fcastReg == this) {
                        fcastReg = null;
                    }
                    if (fcastRetryPending) {
                        return;
                    }
                    fcastRetryPending = true;
                    fcastRetries += 1;
                    long delay = 3_000L << Math.min(fcastRetries - 1, 4);
                    Log.e(TAG, label + " registration failed: " + errorCode
                            + ", retry " + fcastRetries + " in " + delay + "ms");
                    handler.postDelayed(() -> {
                        fcastRetryPending = false;
                        if (!destroyed && fcastReg == null) {
                            fcastPollGen += 1;
                            registerFCastWhenReady(fcastPollGen);
                        }
                    }, delay);
                } else {
                    if (raopReg == this) {
                        raopReg = null;
                    }
                    if (raopRetryPending) {
                        return;
                    }
                    raopRetryPending = true;
                    raopRetries += 1;
                    long delay = 3_000L << Math.min(raopRetries - 1, 4);
                    Log.e(TAG, label + " registration failed: " + errorCode
                            + ", retry " + raopRetries + " in " + delay + "ms");
                    handler.postDelayed(() -> {
                        raopRetryPending = false;
                        if (!destroyed && raopReg == null) {
                            registerRaop();
                        }
                    }, delay);
                }
            });
        }

        @Override
        public void onUnregistrationFailed(NsdServiceInfo info, int errorCode) {
            Log.e(TAG, label + " unregistration failed: " + errorCode);
        }

        @Override
        public void onServiceRegistered(NsdServiceInfo info) {
            Log.i(TAG, label + " registered as " + info.getServiceName());
            handler.post(() -> {
                // per service: raop succeeding must not defeat fcast's
                // backoff or vice versa
                if (isFcast) {
                    fcastRetries = 0;
                    // The daemon renames on collision. The DISPLAYED name
                    // follows the network's truth; the registration base
                    // never moves, or renames would compound.
                    setMdnsDeviceName(info.getServiceName());
                } else {
                    raopRetries = 0;
                }
            });
        }

        @Override
        public void onServiceUnregistered(NsdServiceInfo info) { }
    }

    private void quietUnregister(NsdListener listener) {
        if (listener == null) {
            return;
        }
        try {
            nsdManager.unregisterService(listener);
        } catch (IllegalArgumentException e) {
            // already removed by a failure callback
        }
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
    /// Runs on the net thread.
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
        netHandler.post(this::sweepAddresses);
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
            netHandler.post(MainActivity.this::sweepAddresses);
            handler.postDelayed(this, SWEEP_INTERVAL_MS);
        }
    };

    class NetworkCallbackHandler extends ConnectivityManager.NetworkCallback {
        @Override
        public void onAvailable(@NonNull Network network) {
            handler.post(MainActivity.this::onNetworkChanged);
        }

        @Override
        public void onLost(@NonNull Network network) {
            handler.post(MainActivity.this::onNetworkChanged);
        }

        @Override
        public void onLinkPropertiesChanged(@NonNull Network network,
                @NonNull LinkProperties props) {
            // AP roams, DHCP renews and IPv6 prefix changes keep the same
            // Network and only fire this.
            handler.post(MainActivity.this::onNetworkChanged);
        }
    }

    static {
        // One self-contained library: GStreamer is statically linked inside
        // and initialized by the native side, no java glue involved.
        System.loadLibrary("fcastreceiver");
    }

    private boolean isTelevision() {
        android.app.UiModeManager ui =
                (android.app.UiModeManager) getSystemService(Context.UI_MODE_SERVICE);
        return ui != null && ui.getCurrentModeType()
                == android.content.res.Configuration.UI_MODE_TYPE_TELEVISION;
    }

    @SuppressLint("WakelockTimeout")
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        // Translucent from the first frame. The video hole punch is real
        // per-pixel alpha (the scene clears the hole), and switching later
        // recreates the window surface mid launch, which flashes the
        // launcher through an empty window.
        getWindow().setFormat(android.graphics.PixelFormat.TRANSLUCENT);
        // the theme's windowBackground is only for the starting window, on
        // the live window it would paint over the video hole punch
        getWindow().setBackgroundDrawable(null);

        // Edge-to-edge always: the video rect and slint both measure in
        // full-window pixels, so decor fitting would shift and clip them.
        // Set once and never turned back on.
        if (android.os.Build.VERSION.SDK_INT >= 30) {
            getWindow().setDecorFitsSystemWindows(false);
        }

        // Hardware volume keys drive the media stream; without this they hit
        // the ring stream whenever nothing is actively playing.
        setVolumeControlStream(AudioManager.STREAM_MUSIC);

        createMediaSession();
        ReceiverService.ensureChannel(this);
        // No prompt on TV: no notification shade worth the dialog there.
        if (android.os.Build.VERSION.SDK_INT >= 33 && !isTelevision()
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

        netThread = new HandlerThread("fcast-net");
        netThread.start();
        netHandler = new android.os.Handler(netThread.getLooper());

        nsdManager = (NsdManager) this.getSystemService(Context.NSD_SERVICE);

        String modelName;
        if (android.os.Build.MODEL.contains(android.os.Build.MANUFACTURER)) {
            // quoted: a manufacturer string with regex metacharacters would
            // throw out of onCreate on that device
            modelName = android.os.Build.MODEL
                    .replaceFirst("^" + Pattern.quote(android.os.Build.MANUFACTURER), "")
                    .trim();
        } else {
            modelName = android.os.Build.MODEL;
        }
        baseServiceName = truncateUtf8("FCast-" + android.os.Build.MANUFACTURER + "-" + modelName, 63);

        setMdnsDeviceName(baseServiceName);
        registerServices();

        connectivityManager = (ConnectivityManager) this.getSystemService(Context.CONNECTIVITY_SERVICE);
        NetworkRequest networkRequest = new NetworkRequest.Builder()
                .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
                .addTransportType(NetworkCapabilities.TRANSPORT_ETHERNET)
                .build();
        networkCallback = new NetworkCallbackHandler();
        connectivityManager.registerNetworkCallback(networkRequest, networkCallback);

        netHandler.post(this::sweepAddresses);
        handler.postDelayed(periodicSweep, SWEEP_INTERVAL_MS);

        // Created here, acquired only while playback is active, see
        // setPlaybackActive. Non ref-counted so repeated acquires are
        // idempotent and one release always drops the lock.
        wifiManager = (WifiManager) getApplicationContext().getSystemService(Context.WIFI_SERVICE);
        updateWifiLockMode(true);

        powerManager = (PowerManager) this.getSystemService(Context.POWER_SERVICE);
        cpuWakeLock = powerManager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "FCastRsReceiver:WakeLock");
        cpuWakeLock.setReferenceCounted(false);
    }

    /// DNS-SD instance names cap at 63 bytes of utf8.
    private static String truncateUtf8(String s, int maxBytes) {
        byte[] bytes = s.getBytes(StandardCharsets.UTF_8);
        if (bytes.length <= maxBytes) {
            return s;
        }
        while (maxBytes > 0 && (bytes[maxBytes] & 0xC0) == 0x80) {
            maxBytes--;
        }
        return new String(bytes, 0, maxBytes, StandardCharsets.UTF_8);
    }

    /// (Re-)register both services, dropping any prior registrations first.
    /// The fcast one waits for the TXT records (TLS fingerprint + protocol
    /// version) AND the committed listen port: v4 senders key their secure
    /// connect on the records, and an advertisement pointing at an unbound
    /// port hands early senders a connection refuse.
    private void registerServices() {
        fcastPollGen += 1;
        quietUnregister(fcastReg);
        fcastReg = null;
        quietUnregister(raopReg);
        raopReg = null;

        registerRaop();
        registerFCastWhenReady(fcastPollGen);
    }

    private void registerRaop() {
        String raopHash = getDeviceNameRaopHash(baseServiceName);
        if (raopHash == null) {
            Log.e(TAG, "raop hash unavailable, skipping raop registration");
            return;
        }
        NsdServiceInfo raopServiceInfo = new NsdServiceInfo();
        // the combined instance name also lives under DNS-SD's 63 bytes
        raopServiceInfo.setServiceName(truncateUtf8(raopHash + "@" + baseServiceName, 63));
        raopServiceInfo.setServiceType("_raop._tcp");
        raopServiceInfo.setPort(33505);
        Map<String, String> raopAttrs = new HashMap<>();
        getRaopTxtAttribs(raopAttrs);
        for (Map.Entry<String, String> a : raopAttrs.entrySet()) {
            raopServiceInfo.setAttribute(a.getKey(), a.getValue());
        }
        raopReg = new NsdListener("_raop", false);
        nsdManager.registerService(raopServiceInfo, NsdManager.PROTOCOL_DNS_SD, raopReg);
    }

    private void registerFCastWhenReady(int gen) {
        if (gen != fcastPollGen || destroyed) {
            // a newer registration cycle owns the field now
            return;
        }
        Map<String, String> attrs = new HashMap<>();
        int port = getFCastPort();
        if (port == 0 || !getFCastTxtAttribs(attrs)) {
            handler.postDelayed(() -> registerFCastWhenReady(gen), 200);
            return;
        }
        NsdServiceInfo info = new NsdServiceInfo();
        info.setServiceName(baseServiceName);
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
                // The system already removed this app from the focus stack;
                // dropping the request here is what makes the next resume
                // re-request instead of playing focusless over another app.
                runOnUiThread(() -> {
                    if (focusRequest != null) {
                        AudioManager am = (AudioManager) getSystemService(Context.AUDIO_SERVICE);
                        am.abandonAudioFocusRequest(focusRequest);
                        focusRequest = null;
                    }
                });
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

    /// Request focus if none is held. Called from native code on every
    /// play/load edge: a cast arriving during a phone call must not play
    /// over it, and after a permanent loss the request is gone and needs
    /// remaking. A refusal comes back as a transient-loss event, which
    /// pauses and resumes on the eventual gain.
    public void ensureAudioFocus() {
        runOnUiThread(() -> {
            if (focusRequest != null) {
                return;
            }
            AudioManager am = (AudioManager) getSystemService(Context.AUDIO_SERVICE);
            android.media.AudioFocusRequest req =
                    new android.media.AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
                            .setAudioAttributes(new android.media.AudioAttributes.Builder()
                                    .setUsage(android.media.AudioAttributes.USAGE_MEDIA)
                                    .setContentType(android.media.AudioAttributes.CONTENT_TYPE_MOVIE)
                                    .build())
                            .setOnAudioFocusChangeListener(focusListener)
                            .setWillPauseWhenDucked(true)
                            .build();
            int result = am.requestAudioFocus(req);
            if (result == AudioManager.AUDIOFOCUS_REQUEST_GRANTED) {
                focusRequest = req;
            } else {
                Log.i(TAG, "audio focus refused (" + result + ")");
                nativeAudioEvent(1);
            }
        });
    }

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
        // lock-screen/QS media card tap opens the app
        Intent open = new Intent(this, MainActivity.class);
        open.setFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_SINGLE_TOP);
        mediaSession.setSessionActivity(android.app.PendingIntent.getActivity(
                this, 0, open, android.app.PendingIntent.FLAG_IMMUTABLE));
        ReceiverService.sessionToken = mediaSession.getSessionToken();
    }

    private boolean castPlaying = false;

    /// The CPU wake lock follows actually-playing, not merely
    /// session-active: a cast paused overnight with the screen off would
    /// otherwise hold a partial wake lock for hours, which is exactly what
    /// Play's excessive-wake-lock quality threshold flags. Playing audio
    /// under the mediaPlayback FGS is the policy's exempt case.
    @SuppressLint("WakelockTimeout")
    private void syncWakeLock() {
        boolean want = castActiveLocal && castPlaying;
        if (want && !cpuWakeLock.isHeld()) {
            cpuWakeLock.acquire();
        } else if (!want && cpuWakeLock.isHeld()) {
            cpuWakeLock.release();
        }
    }

    private boolean castActiveLocal = false;
    private boolean visible = false;

    /// A cast arriving while the activity is backgrounded should put the
    /// receiver on screen, the whole point of a TV cast target. Background
    /// activity starts need the overlay permission (the Kotlin receiver
    /// ships the same way); without it the FGS notification's tap-to-open
    /// stays the only route and the attempt is just skipped.
    private void bringToFront() {
        Intent i = new Intent(this, MainActivity.class);
        i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_SINGLE_TOP);
        try {
            if (android.provider.Settings.canDrawOverlays(this)) {
                PendingIntent.getActivity(this, 2, i,
                        PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE)
                        .send();
            } else {
                // blocked from the background on 10+, works when the app is
                // merely covered rather than stopped
                startActivity(i);
            }
        } catch (Exception e) {
            Log.w(TAG, "bring-to-front refused", e);
        }
    }

    /// Called from native code on state edges and a 1 Hz keepalive.
    /// Any thread; MediaSession is thread-safe.
    @SuppressLint("WakelockTimeout")
    public void updateMediaSession(boolean playing, long positionMs, float speed) {
        runOnUiThread(() -> {
            castPlaying = playing;
            syncWakeLock();
        });
        MediaSession session = mediaSession;
        if (session == null) {
            return;
        }
        android.media.session.PlaybackState.Builder b =
                new android.media.session.PlaybackState.Builder()
                        .setActions(android.media.session.PlaybackState.ACTION_PLAY
                                | android.media.session.PlaybackState.ACTION_PAUSE
                                | android.media.session.PlaybackState.ACTION_PLAY_PAUSE
                                | android.media.session.PlaybackState.ACTION_STOP
                                | android.media.session.PlaybackState.ACTION_SEEK_TO)
                        // paused state must not extrapolate: the platform
                        // advances position at the given speed
                        .setState(playing
                                        ? android.media.session.PlaybackState.STATE_PLAYING
                                        : android.media.session.PlaybackState.STATE_PAUSED,
                                positionMs, playing ? speed : 0f);
        session.setPlaybackState(b.build());
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
        MediaSession session = mediaSession;
        if (session != null) {
            session.setMetadata(new android.media.MediaMetadata.Builder()
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
            if (active && !visible) {
                bringToFront();
            }
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
            castVisual = active && visual;

            AudioManager am = (AudioManager) getSystemService(Context.AUDIO_SERVICE);
            castActiveLocal = active;
            if (active) {
                // playing usually follows within a tick; acquiring here too
                // covers the load window, syncWakeLock drops it on pause
                castPlaying = true;
                syncWakeLock();
                wifiLock.acquire();
                ensureAudioFocus();
                if (noisyReceiver == null) {
                    noisyReceiver = new android.content.BroadcastReceiver() {
                        @Override
                        public void onReceive(Context context, android.content.Intent intent) {
                            nativeAudioEvent(3);
                        }
                    };
                    if (android.os.Build.VERSION.SDK_INT >= 33) {
                        registerReceiver(noisyReceiver,
                                new android.content.IntentFilter(AudioManager.ACTION_AUDIO_BECOMING_NOISY),
                                Context.RECEIVER_NOT_EXPORTED);
                    } else {
                        registerReceiver(noisyReceiver,
                                new android.content.IntentFilter(AudioManager.ACTION_AUDIO_BECOMING_NOISY));
                    }
                }
            } else {
                castPlaying = false;
                syncWakeLock();
                // release() on an unheld lock throws, and the idle edge can
                // fire without a preceding active one (teardown).
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

    private volatile boolean immersiveWanted = false;

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
            // decor fitting stays OFF: turning it back on shifts the video
            // rect and slint's coordinate space by the bar insets
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

    private boolean castVisual = false;
    private volatile android.util.Rational videoAspect = new android.util.Rational(16, 9);

    /// The current video's aspect, from native code at relayout. Clamped to
    /// PiP's accepted 0.418..2.39 range.
    public void setVideoAspect(int w, int h) {
        if (w <= 0 || h <= 0) {
            return;
        }
        float r = (float) w / h;
        if (r < 0.42f) {
            videoAspect = new android.util.Rational(42, 100);
        } else if (r > 2.38f) {
            videoAspect = new android.util.Rational(238, 100);
        } else {
            videoAspect = new android.util.Rational(w, h);
        }
    }

    /// Home during visual playback shrinks to picture-in-picture instead of
    /// hiding the video. PiP resizes without onStop, so the surface and the
    /// direct codec session survive; the relayout refits on the new bounds.
    @Override
    protected void onUserLeaveHint() {
        super.onUserLeaveHint();
        if (castVisual && !isInPictureInPictureMode()) {
            try {
                enterPictureInPictureMode(
                        new android.app.PictureInPictureParams.Builder()
                                .setAspectRatio(videoAspect)
                                .build());
            } catch (IllegalStateException | IllegalArgumentException e) {
                // devices can configure tighter aspect limits than AOSP's
                Log.w(TAG, "PiP refused", e);
            }
        }
    }

    @Override
    public void onPictureInPictureModeChanged(boolean inPip,
            android.content.res.Configuration newConfig) {
        super.onPictureInPictureModeChanged(inPip, newConfig);
        // Swiping the PiP window away finishes the activity, which takes
        // the whole process with it (exit-on-destroy); at least end the
        // cast cleanly so senders learn instead of timing out.
        if (!inPip && isFinishing()) {
            nativeMediaCommand(0);
        }
    }

    @Override
    protected void onNewIntent(Intent intent) {
        // singleTask: notification and session taps land here instead of
        // spawning a second activity, which the singleton native main
        // could not serve.
        super.onNewIntent(intent);
    }

    /// The video SurfaceView's surface dies with the activity; the native
    /// side detaches it from the player before that and re-adopts a fresh
    /// one on return, otherwise a running codec errors out mid-video.
    native void nativeAppVisibility(boolean visible);

    /// LOW_LATENCY only bites while foreground with the screen on and
    /// silently degrades in the background, exactly where a backgrounded
    /// cast needs wifi kept awake: swap modes on the visibility edge.
    private void updateWifiLockMode(boolean foreground) {
        int mode = (foreground && android.os.Build.VERSION.SDK_INT >= 29)
                ? WifiManager.WIFI_MODE_FULL_LOW_LATENCY
                : WifiManager.WIFI_MODE_FULL_HIGH_PERF;
        // An idle receiver must answer the moment a sender connects, so the
        // lock is held whenever the activity is up, not only while casting.
        // Without it the radio power saves and LAN round trips balloon to
        // ~500ms, blowing sender handshake deadlines (seen on the LEAP-S1).
        // Backgrounded, only a running cast justifies keeping it.
        boolean want = foreground || castActiveLocal;
        if (wifiLock == null || mode != wifiLockMode) {
            if (wifiLock != null && wifiLock.isHeld()) {
                wifiLock.release();
            }
            wifiLock = wifiManager.createWifiLock(mode, "FCastRsReceiver:WifiLock");
            wifiLock.setReferenceCounted(false);
            wifiLockMode = mode;
        }
        if (want && !wifiLock.isHeld()) {
            wifiLock.acquire();
        } else if (!want && wifiLock.isHeld()) {
            wifiLock.release();
        }
    }

    private int wifiLockMode = -1;

    @Override
    protected void onStart() {
        super.onStart();
        visible = true;
        // The activity is back; its own lifecycle keeps the process warm.
        stopService(new Intent(this, ReceiverService.class));
        updateWifiLockMode(true);
        nativeAppVisibility(true);
    }

    @Override
    protected void onStop() {
        visible = false;
        updateWifiLockMode(false);
        nativeAppVisibility(false);
        // Backgrounded: without foreground priority the process is a cached
        // kill candidate and the NSD registration dies with it. Started
        // here, inside the background-start grace window; the system can
        // still refuse (doze, restricted bucket), which must not crash a
        // running cast.
        if (!destroyed && !isFinishing()) {
            try {
                startForegroundService(new Intent(this, ReceiverService.class));
            } catch (Exception e) {
                Log.w(TAG, "foreground service refused", e);
            }
        }
        super.onStop();
    }

    @Override
    protected void onDestroy() {
        // Before super: NativeActivity's onDestroy blocks on the native
        // thread, which exits the process, so anything after it never runs.
        destroyed = true;
        handler.removeCallbacksAndMessages(null);
        quietUnregister(fcastReg);
        fcastReg = null;
        quietUnregister(raopReg);
        raopReg = null;
        if (networkCallback != null) {
            connectivityManager.unregisterNetworkCallback(networkCallback);
            networkCallback = null;
        }
        if (netThread != null) {
            netThread.quitSafely();
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

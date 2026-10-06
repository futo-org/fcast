package org.fcast.rsreceiver.android;

import android.content.Intent;
import android.media.AudioManager;
import android.os.Bundle;
import android.app.NativeActivity;
import android.util.Log;
import androidx.annotation.NonNull;

/// The receiver's window: the slint UI, PiP, immersive mode and the
/// permission prompts. Everything that must outlive the window lives in
/// ReceiverCore; this activity only attaches to it.
public class MainActivity extends NativeActivity {
    private static final String TAG = "FCastMainActivity";

    // Last soft keyboard state reported below API 30.
    private boolean keyboardShown = false;
    private boolean destroyed = false;
    /// Between onStop and onStart. A PiP window closed on 9 to 12 stops the
    /// activity before the mode change, where an expand starts it.
    private boolean stopped = false;
    /// This instance has presented a receiver frame. SplashActivity skips its
    /// art while it holds.
    static volatile boolean painted = false;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        // Before super: NativeActivity starts the native thread there, and the
        // core it attaches to must exist (it outlives this activity).
        ReceiverCore.ensureStarted(this);
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

        ReceiverCore.attachActivity(this);
        Updater.onActivityCreated(this);
        // TVs too: tap-to-play, the boot and the update notices are the
        // fallback when the overlay grant is missing.
        if (android.os.Build.VERSION.SDK_INT >= 33
                && checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS)
                        != android.content.pm.PackageManager.PERMISSION_GRANTED) {
            // the overlay prompt waits for this one's answer
            requestPermissions(
                    new String[] { android.Manifest.permission.POST_NOTIFICATIONS }, 1);
        } else {
            runPrompts();
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
        //
        // The soft keyboard's comings and goings do need one: a text field
        // left in edit mode after the user dismissed the keyboard shows no
        // keyboard again and answers no key. Watched on the decor view, an
        // ancestor of slint's input view, which keeps its own callback; the
        // subtree dispatch mode lets both run.
        if (android.os.Build.VERSION.SDK_INT >= 30) {
            getWindow().getDecorView().setWindowInsetsAnimationCallback(
                    new android.view.WindowInsetsAnimation.Callback(
                            android.view.WindowInsetsAnimation.Callback.DISPATCH_MODE_CONTINUE_ON_SUBTREE) {
                        @Override
                        public android.view.WindowInsets onProgress(
                                android.view.WindowInsets insets,
                                java.util.List<android.view.WindowInsetsAnimation> running) {
                            return insets;
                        }

                        @Override
                        public void onEnd(android.view.WindowInsetsAnimation animation) {
                            if ((animation.getTypeMask() & android.view.WindowInsets.Type.ime()) == 0) {
                                return;
                            }
                            android.view.WindowInsets root = getWindow().getDecorView().getRootWindowInsets();
                            nativeSoftKeyboardVisible(
                                    root != null && root.isVisible(android.view.WindowInsets.Type.ime()));
                        }
                    });
        } else {
            // No ime insets before 30: a keyboard is the visible frame
            // losing more than the nav bar's share of the window.
            final android.view.View decor = getWindow().getDecorView();
            final android.graphics.Rect visible = new android.graphics.Rect();
            final int[] origin = new int[2];
            decor.getViewTreeObserver().addOnGlobalLayoutListener(() -> {
                // The frame is in screen coordinates: in split screen or a
                // freeform window the decor's top is not 0, so measure the
                // frame's bottom from it or the window never reads covered.
                decor.getWindowVisibleDisplayFrame(visible);
                decor.getLocationOnScreen(origin);
                int height = decor.getRootView().getHeight();
                int covered = height - (visible.bottom - origin[1]);
                boolean shown = height > 0 && covered > height * 15 / 100;
                if (shown != keyboardShown) {
                    keyboardShown = shown;
                    nativeSoftKeyboardVisible(shown);
                }
            });
        }
    }

    /// The soft keyboard came up (true) or went away (false).
    static native void nativeSoftKeyboardVisible(boolean visible);

    /// The screen pin, ReceiverCore.keepScreenOn decides. From ReceiverCore
    /// on the main thread.
    void applyKeepScreenOn(boolean on) {
        if (on) {
            getWindow().addFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        } else {
            getWindow().clearFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        }
    }

    private static final String PREFS = "receiver";
    private static final String KEY_OVERLAY_PROMPTED = "overlay_prompted";

    /// The first-launch questions, one after the other.
    private void runPrompts() {
        if (!maybePromptOverlayPermission(this::maybePromptStartOnBoot)) {
            maybePromptStartOnBoot();
        }
    }

    /// Asks once for the overlay grant, the only way a cast arriving in the
    /// background can open the receiver by itself. Not every TV build has a
    /// settings page for it, then there is nothing to ask. True when asked,
    /// `then` runs once the dialog is gone.
    private boolean maybePromptOverlayPermission(Runnable then) {
        if (hasOverlayPermission()) {
            return false;
        }
        android.content.SharedPreferences prefs = getSharedPreferences(PREFS, MODE_PRIVATE);
        if (prefs.getBoolean(KEY_OVERLAY_PROMPTED, false)) {
            return false;
        }
        if (!canOpenOverlaySettings()) {
            return false;
        }
        prefs.edit().putBoolean(KEY_OVERLAY_PROMPTED, true).apply();
        new android.app.AlertDialog.Builder(this,
                android.R.style.Theme_DeviceDefault_Dialog_Alert)
                .setTitle(R.string.overlay_prompt_title)
                .setMessage(R.string.overlay_prompt_message)
                .setPositiveButton(R.string.overlay_prompt_allow, (d, w) -> openOverlaySettings())
                .setNegativeButton(R.string.overlay_prompt_later, null)
                .setOnDismissListener(d -> then.run())
                .show();
        return true;
    }

    /// Start on boot is asked on first launch, every device: off until the
    /// user answers, so no dismissing it.
    private void maybePromptStartOnBoot() {
        if (destroyed || !ReceiverCore.startOnBootUnasked()) {
            return;
        }
        new android.app.AlertDialog.Builder(this,
                android.R.style.Theme_DeviceDefault_Dialog_Alert)
                .setTitle(R.string.boot_prompt_title)
                .setMessage(R.string.boot_prompt_message)
                .setPositiveButton(R.string.boot_prompt_yes, (d, w) -> ReceiverCore.answerStartOnBoot(true))
                .setNegativeButton(R.string.boot_prompt_no, (d, w) -> ReceiverCore.answerStartOnBoot(false))
                .setCancelable(false)
                .show();
    }

    private Intent overlaySettingsIntent() {
        return new Intent(android.provider.Settings.ACTION_MANAGE_OVERLAY_PERMISSION,
                android.net.Uri.parse("package:" + getPackageName()));
    }

    /// Whether a background cast can bring the receiver up. Background
    /// activity starts are only restricted from 29, before that nothing
    /// needs granting, so neither the prompt nor the drawer row asks.
    private boolean hasOverlayPermission() {
        return android.os.Build.VERSION.SDK_INT < 29
                || android.provider.Settings.canDrawOverlays(this);
    }

    private boolean canOpenOverlaySettings() {
        return overlaySettingsIntent().resolveActivity(getPackageManager()) != null;
    }

    /// The settings drawer's overlay row, from native code on any thread.
    public void openOverlaySettings() {
        runOnUiThread(() -> {
            try {
                startActivity(overlaySettingsIntent());
            } catch (android.content.ActivityNotFoundException e) {
                Log.w(TAG, "no overlay permission settings", e);
            }
        });
    }

    @Override
    public void onRequestPermissionsResult(int requestCode, @NonNull String[] permissions,
            @NonNull int[] grantResults) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults);
        if (requestCode == 1) {
            runPrompts();
        }
    }

    /// Refresh-rate matching: prefer the lowest display mode at the current
    /// resolution whose rate is a near-integer multiple of the content fps
    /// (23.976 picks 24 or 120, never 60). 0 restores no-preference. From
    /// ReceiverCore on the main thread.
    void applyContentFrameRate(float fps) {
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
        // Focus, not onStart: TV settings open as a side panel that only
        // pauses the activity, and the prompt dialog never stops it.
        if (hasFocus) {
            nativeOverlayState(hasOverlayPermission(), canOpenOverlaySettings());
        }
    }

    static native void nativeOverlayState(boolean granted, boolean canOpenSettings);

    /// A cast that ended for real: the idle UI does not belong in a video
    /// window (PiP), nor on screen when the cast is what brought the
    /// receiver up (`fromBackground`). To the back rather than finish, so
    /// the window survives for the next cast. From ReceiverCore on the main
    /// thread.
    void leaveForEndedCast(boolean fromBackground) {
        // Hidden from here, not from onStop: a cast landing before onStop
        // must still bring the receiver back up.
        if (leavesForEndedCast(fromBackground) && moveTaskToBack(true)) {
            ReceiverCore.setUiVisible(false);
        }
    }

    /// Whether leaveForEndedCast will send the task to the back.
    boolean leavesForEndedCast(boolean fromBackground) {
        return !destroyed && (fromBackground || isInPictureInPictureMode());
    }

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
        if (ReceiverCore.castVisual() && !isInPictureInPictureMode()) {
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
        // Closing the PiP window either finishes the activity or, on 9 to 12,
        // only stops it behind Home. Either way the user dismissed the video:
        // end the cast for the senders, and keep the receiver reachable as
        // Home would, a finishing onStop starts no service.
        if (!inPip && (isFinishing() || stopped)) {
            ReceiverCore.nativeMediaCommand(0);
            keepReceiverUp();
        }
    }

    /// The service keeps the process and its discovery alive without a
    /// visible activity. The system can refuse the start (doze, restricted
    /// bucket), which must not crash.
    private void keepReceiverUp() {
        try {
            startForegroundService(new Intent(this, ReceiverService.class));
            ReceiverCore.setServiceWanted(true);
        } catch (Exception e) {
            Log.w(TAG, "foreground service refused", e);
        }
    }

    @Override
    protected void onNewIntent(Intent intent) {
        // singleTask: notification and session taps land here instead of
        // spawning a second activity.
        super.onNewIntent(intent);
    }

    /// The video SurfaceView's surface dies with the activity; the native
    /// side detaches it from the player before that and re-adopts a fresh
    /// one on return, otherwise a running codec errors out mid-video.
    native void nativeAppVisibility(boolean visible);

    @Override
    protected void onStart() {
        super.onStart();
        stopped = false;
        ReceiverCore.setUiVisible(true);
        Updater.onActivityStarted(this);
        // The activity is back; its own lifecycle keeps the process warm,
        // unless start on boot keeps the service up for good.
        if (!ReceiverCore.startOnBoot()) {
            ReceiverCore.setServiceWanted(false);
            stopService(new Intent(this, ReceiverService.class));
        }
        nativeAppVisibility(true);
    }

    @Override
    protected void onResume() {
        super.onResume();
        // A cast landing while an ended cast's leave animates brings the
        // task back before onStop, so onStart never re-marks it visible.
        ReceiverCore.setUiVisible(true);
    }

    @Override
    protected void onStop() {
        stopped = true;
        ReceiverCore.setUiVisible(false);
        Updater.onActivityStopped();
        nativeAppVisibility(false);
        // Backgrounded: without foreground priority the process is a cached
        // kill candidate and the NSD registration dies with it. Started
        // here, inside the background-start grace window; the system can
        // still refuse (doze, restricted bucket), which must not crash a
        // running cast.
        if (!destroyed && !isFinishing()) {
            keepReceiverUp();
        }
        super.onStop();
    }

    /// The receiver's first rendered frame, from the slint thread. The render
    /// precedes its present, so the splash retires a vsync later.
    public void onReceiverPainted() {
        runOnUiThread(() -> android.view.Choreographer.getInstance().postFrameCallback(t -> {
            if (!destroyed) {
                painted = true;
            }
            SplashActivity.retire();
        }));
    }

    @Override
    protected void onDestroy() {
        // Only the window goes. The core, its discovery and the service stay
        // up, the native side decides whether the process ends with it.
        destroyed = true;
        painted = false;
        ReceiverCore.detachActivity(this);
        super.onDestroy();
    }
}

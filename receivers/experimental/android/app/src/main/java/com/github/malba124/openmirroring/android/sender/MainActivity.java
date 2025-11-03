package com.github.malba124.openmirroring.android.sender;

import android.os.Bundle;
import android.app.NativeActivity;
import android.util.Log;

import org.freedesktop.gstreamer.GStreamer;

public class MainActivity extends NativeActivity {
    static {
        System.loadLibrary("gstreamer_android");
        System.loadLibrary("openmirroringreceiver");
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        try {
            GStreamer.init(this);
        } catch (Exception e) {
            Log.e("MAIN_ACTIVITY", "Failed to init GStreamer ${e}");
            finish();
        }
    }
}

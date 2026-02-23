/**
 * Copy this file into your Android project and call init(). If your project
 * contains fonts and/or certificates in assets, uncomment copyFonts() and/or
 * copyCaCertificates() lines in init().
 */
package org.freedesktop.gstreamer;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;

import android.content.Context;
import android.content.res.AssetManager;
import android.system.Os;

public class GStreamer {
    private static native void nativeInit(Context context) throws Exception;

    public static void init(Context context) throws Exception {
        nativeInit(context);
    }
}

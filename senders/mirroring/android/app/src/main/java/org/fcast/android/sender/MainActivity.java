package org.fcast.android.sender;

import android.app.NativeActivity;
import android.content.Context;
import android.content.Intent;
import android.graphics.PixelFormat;
import android.hardware.display.DisplayManager;
import android.hardware.display.VirtualDisplay;
import android.media.Image;
import android.media.ImageReader;
import android.media.projection.MediaProjection;
import android.media.projection.MediaProjectionManager;
import android.net.nsd.NsdManager;
import android.net.nsd.NsdServiceInfo;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.util.DisplayMetrics;
import android.util.Log;

import androidx.annotation.NonNull;

import com.journeyapps.barcodescanner.ScanOptions;

import org.freedesktop.gstreamer.GStreamer;

import java.net.Inet6Address;
import java.net.InetAddress;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.List;
import java.util.concurrent.locks.ReentrantLock;
import java.util.stream.Collectors;

class FCastDiscoveryListener implements NsdManager.DiscoveryListener {
    private static final String TAG = "FCastDiscoveryListener";
    private final NsdManager nsdManager;


    FCastDiscoveryListener(NsdManager nsdManager) {
        this.nsdManager = nsdManager;
    }

    private static ByteBuffer addrConvert(InetAddress addr) {
        byte[] addrB = addr.getAddress();
        ByteBuffer buffer = ByteBuffer.allocateDirect(addrB.length);
        buffer.put(addrB);

        if (addr.getClass() == Inet6Address.class) {
            int scopeId = ((Inet6Address) addr).getScopeId();
            buffer.order(ByteOrder.LITTLE_ENDIAN).putInt(scopeId);
        }

        return buffer;
    }

    @Override
    public void onStartDiscoveryFailed(String serviceType, int errorCode) {
        Log.e(TAG, "Failed to start discovery errorCode=" + errorCode);
    }

    @Override
    public void onStopDiscoveryFailed(String serviceType, int errorCode) {
        Log.e(TAG, "Failed to stop discovery errorCode=" + errorCode);
    }

    @Override
    public void onDiscoveryStarted(String serviceType) {
        Log.i(TAG, "Discovery started");
    }

    @Override
    public void onDiscoveryStopped(String serviceType) {
        Log.i(TAG, "Discovery stopped");
    }

    @Override
    public void onServiceFound(NsdServiceInfo serviceInfo) {
        Log.i(TAG, "Service found serviceInfo=" + serviceInfo);

        List<InetAddress> addrs = List.of();
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            addrs = serviceInfo.getHostAddresses();
        } else {
            InetAddress hostAddr = serviceInfo.getHost();
            if (hostAddr != null) {
                addrs = List.of(hostAddr);
            }
        }
        List<ByteBuffer> addrsB = addrs.stream().map(FCastDiscoveryListener::addrConvert).collect(Collectors.toList());
        serviceFound(serviceInfo.getServiceName(), addrsB, serviceInfo.getPort());

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            nsdManager.registerServiceInfoCallback(serviceInfo, Runnable::run, new NsdManager.ServiceInfoCallback() {
                @Override
                public void onServiceInfoCallbackRegistrationFailed(int errorCode) {
                }

                @Override
                public void onServiceUpdated(@NonNull NsdServiceInfo serviceInfo) {
                    serviceFound(serviceInfo.getServiceName(), serviceInfo.getHostAddresses().stream().map(FCastDiscoveryListener::addrConvert).collect(Collectors.toList()), serviceInfo.getPort());
                }

                @Override
                public void onServiceLost() {
                    serviceLost(serviceInfo.getServiceName());
                }

                @Override
                public void onServiceInfoCallbackUnregistered() {
                }
            });
        } else {
            nsdManager.resolveService(serviceInfo, new NsdManager.ResolveListener() {
                @Override
                public void onResolveFailed(NsdServiceInfo serviceInfo, int errorCode) {
                    Log.e(TAG, "Service failed to resolve serviceInfo=" + serviceInfo);
                }

                @Override
                public void onServiceResolved(NsdServiceInfo serviceInfo) {
                    Log.i(TAG, "Service resolved serviceInfo=" + serviceInfo);
                    InetAddress addr = serviceInfo.getHost();
                    if (addr != null) {
                        serviceFound(serviceInfo.getServiceName(), List.of(addrConvert(addr)), serviceInfo.getPort());
                    }
                }
            });
        }
    }

    @Override
    public void onServiceLost(NsdServiceInfo serviceInfo) {
        Log.i(TAG, "Service lost serviceInfo=" + serviceInfo);
        serviceLost(serviceInfo.getServiceName());
    }

    private native void serviceFound(String name, List<ByteBuffer> addrs, int port);

    private native void serviceLost(String name);
}

class Discoverer {
    public Discoverer(Context context) {
        NsdManager nsdManager = (NsdManager) context.getSystemService(Context.NSD_SERVICE);
        nsdManager.discoverServices("_fcast._tcp", NsdManager.PROTOCOL_DNS_SD, new FCastDiscoveryListener(nsdManager));
    }
}

public class MainActivity extends NativeActivity {
    public static final String ACTION_RESULT = "org.fcast.android.sender.SCREEN_CAPTURE_RESULT";
    private static final int REQUEST_CODE = 1;
    private static final int QR_SCAN_REQUEST_CODE = 2;
    private static final String TAG = "MainActivity";

    static {
        System.loadLibrary("gstreamer_android");
        System.loadLibrary("fcastmirroringsender");
    }

    private final ReentrantLock imageReaderLock = new ReentrantLock();
    private Handler handler;
    private ProjectionCallback projectionCallback;
    private MediaProjectionManager mediaProjectionManager;
    private MediaProjection mediaProjection;
    private ImageReader imageReader;
    private VirtualDisplay virtualDisplay;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        try {
            GStreamer.init(this);
        } catch (Exception e) {
            Log.e(TAG, "Failed to init GStreamer ${e}");
            finish();
        }

        Discoverer discoverer = new Discoverer(this);

        handler = new Handler(Looper.getMainLooper());
        projectionCallback = new MainActivity.ProjectionCallback();
        mediaProjectionManager = (MediaProjectionManager) getSystemService(MEDIA_PROJECTION_SERVICE);
    }

    private void setupVirtualDisplay() {
        DisplayMetrics metrics = getResources().getDisplayMetrics();
        int width = metrics.widthPixels;
        int height = metrics.heightPixels;
        int density = metrics.densityDpi;

        imageReader = ImageReader.newInstance(width, height, PixelFormat.RGBA_8888, 2);
        imageReader.setOnImageAvailableListener(reader -> {
            // NOTE: lock so the image reader isn't closed while the native routine is copying the buffer (segfaults if not)
            imageReaderLock.lock();
            try (Image image = reader.acquireLatestImage()) {
                if (image == null) {
                    return;
                }

                Image.Plane[] planes = image.getPlanes();
                ByteBuffer buffer = planes[0].getBuffer();
                int pixelStride = planes[0].getPixelStride();
                int rowStride = planes[0].getRowStride();
                int iWidth = image.getWidth();
                int iHeight = image.getHeight();
                nativeProcessFrame(buffer, iWidth, iHeight, pixelStride, rowStride);
            } catch (Exception e) {
                Log.e(TAG, "Failed to process image: " + e);
            } finally {
                imageReaderLock.unlock();
            }
        }, handler);

        virtualDisplay = mediaProjection.createVirtualDisplay("ScreenCapture", width, height, density, DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR, imageReader.getSurface(), null, handler);
    }

    // Called from native code
    private void startScreenCapture() {
        MediaProjectionManager projectionManager = (MediaProjectionManager) getSystemService(Context.MEDIA_PROJECTION_SERVICE);
        startActivityForResult(projectionManager.createScreenCaptureIntent(), REQUEST_CODE);
    }

    // Called from native code
    private void stopCapture() {
        if (virtualDisplay == null && imageReader == null && mediaProjection == null) {
            // Already stopped
            return;
        }
        if (imageReader != null) {
            imageReaderLock.lock();
            imageReader.close();
            imageReader = null;
            Log.d(TAG, "Image reader closed");
            imageReaderLock.unlock();
        }
        if (virtualDisplay != null) {
            virtualDisplay.release();
            virtualDisplay = null;
            Log.d(TAG, "Virtual display released");
        }
        if (mediaProjection != null) {
            mediaProjection.stop();
            mediaProjection = null;
            Log.d(TAG, "Media projection stopped");
        }

        nativeCaptureStopped();
    }

    // Called from native code
    private void scanQr() {
        ScanOptions options = new ScanOptions();
        options.setDesiredBarcodeFormats(ScanOptions.QR_CODE);
        // NOTE: crashes if scan succeeds and the screen is oriented differently than what it was when the scan activity was started...
        options.setOrientationLocked(true);
        Intent intent = options.createScanIntent(this);
        startActivityForResult(intent, QR_SCAN_REQUEST_CODE);
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode == REQUEST_CODE && resultCode == RESULT_OK) {
            Intent intent = new Intent(this, ScreenCaptureService.class);
            intent.setAction(ACTION_RESULT);
            intent.putExtra("resultCode", resultCode);
            intent.putExtra("data", data);
            startForegroundService(intent);

            Log.d(TAG, "starting screen capture");

            mediaProjection = mediaProjectionManager.getMediaProjection(resultCode, data);
            mediaProjection.registerCallback(projectionCallback, null);

            setupVirtualDisplay();

            nativeCaptureStarted();
        } else if (requestCode == REQUEST_CODE && resultCode == RESULT_CANCELED) {
            Log.d(TAG, "Media projection Canceled");
            nativeCaptureCancelled();
        } else if (requestCode == QR_SCAN_REQUEST_CODE && resultCode == RESULT_OK) {
            String result = data.getStringExtra("SCAN_RESULT");
            nativeQrScanResult(result);
        }
    }

    native void nativeProcessFrame(ByteBuffer buffer, int width, int height, int pixelStride, int rowStride);

    native void nativeCaptureStarted();

    native void nativeCaptureStopped();

    native void nativeCaptureCancelled();

    native void nativeQrScanResult(String result);

    public class ProjectionCallback extends MediaProjection.Callback {
        @Override
        public void onStop() {
            stopCapture();
        }

        @Override
        public void onCapturedContentResize(int width, int height) {
            // TODO: does this work? Need to test on a device with API level 34
            virtualDisplay.resize(width, height, getResources().getDisplayMetrics().densityDpi);
        }
    }
}
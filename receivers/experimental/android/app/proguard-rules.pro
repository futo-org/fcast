# Native code calls these by name and signature; minification must not
# rename or strip them. minifyEnabled is off today, these keeps are the
# insurance for the day it flips.
-keepclasseswithmembers class org.fcast.rsreceiver.android.MainActivity {
    native <methods>;
    public *;
}
-keep class org.fcast.rsreceiver.android.ReceiverService { *; }
-keep class SlintVideoSurface { public *; }
-keep class SlintAndroidJavaHelper { *; }

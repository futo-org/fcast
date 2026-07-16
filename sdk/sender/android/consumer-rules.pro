# JNA runtime
-keep class com.sun.jna.** { *; }
-keepclassmembers class * extends com.sun.jna.Structure { *; }
-keep class * implements com.sun.jna.Callback { *; }
-dontwarn java.awt.**

# UniFFI-generated bindings
-keep class org.fcast.sender_sdk.** { *; }

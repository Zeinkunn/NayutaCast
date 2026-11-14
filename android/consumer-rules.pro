# Keep JNI callback signatures invoked by Rust libnayutacast.so
-keep class com.nayutacast.** { *; }
-keepclassmembers class com.nayutacast.NayutacastReceiverAdapterObserver {
    public boolean sink(int, int, long, byte[]);
    public void close();
}
-keepclassmembers class com.nayutacast.DiscoveryServiceObserver {
    public void onLine(java.lang.String, java.lang.String, java.lang.String);
    public void offLine(java.lang.String, java.lang.String, java.lang.String);
    public void onMessage(java.lang.String, java.lang.String, java.lang.String, byte[]);
}
-keepclassmembers class com.nayutacast.StreamBufferInfo {
    *;
}
-keepclassmembers class * {
    native <methods>;
}

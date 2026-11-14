# Keep JNI callback classes & kotlinx.serialization serializers under R8
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
-keepattributes *Annotation*, InnerClasses
-dontnote kotlinx.serialization.SerializationKt
-keep,includedescriptorclasses class com.nayutacast.app.**$$serializer { *; }
-keepclassmembers class com.nayutacast.app.** {
    *** Companion;
}
-keepclasseswithmembers class com.nayutacast.app.** {
    kotlinx.serialization.KSerializer serializer(...);
}

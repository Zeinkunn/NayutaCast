plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)

    kotlin("plugin.serialization") version libs.versions.kotlin.get()
}

android {
    namespace = "com.nayutacast.app"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.nayutacast.app"
        minSdk = 29
        targetSdk = 35
        versionCode = 6
        versionName = "0.4.2"
    }

    val releaseKsPath = System.getenv("ANDROID_KEYSTORE_PATH")
        ?.takeIf { it.isNotBlank() && file(it).exists() }

    if (releaseKsPath != null) {
        signingConfigs {
            create("release") {
                storeFile = file(releaseKsPath)
                storePassword = System.getenv("ANDROID_KEYSTORE_PASSWORD") ?: "nayutacast"
                keyAlias = System.getenv("ANDROID_KEY_ALIAS") ?: "nayutacast"
                keyPassword = System.getenv("ANDROID_KEY_PASSWORD") ?: "nayutacast"
            }
        }
    }

    buildTypes {
        release {
            isDebuggable = false
            isMinifyEnabled = true
            isShrinkResources = true
            signingConfig = if (releaseKsPath != null) {
                signingConfigs.getByName("release")
            } else {
                signingConfigs.getByName("debug")
            }
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_11
        targetCompatibility = JavaVersion.VERSION_11
    }

    kotlinOptions {
        jvmTarget = "11"
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }
}

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.ui)
    implementation(libs.androidx.ui.graphics)
    implementation(libs.androidx.ui.tooling.preview)
    implementation(libs.androidx.material3)
    implementation(libs.kotlinx.serialization.json)
    implementation(project(":nayutacast"))
}

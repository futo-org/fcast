plugins {
    alias(libs.plugins.android.library)
    alias(libs.plugins.kotlin.android)
}

android {
    namespace = "fcast.sender_sdk"
    compileSdk = 35

    defaultConfig {
        minSdk = 21
        targetSdk = 34
    }

    sourceSets {
        getByName("main") {
            java.srcDirs("src")
            jniLibs.srcDir("src/jniLibs")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_11
        targetCompatibility = JavaVersion.VERSION_11
    }
    kotlinOptions {
        jvmTarget = "11"
    }
}

dependencies {
    implementation("net.java.dev.jna:jna:5.12.0@aar")
}

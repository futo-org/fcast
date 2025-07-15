plugins {
    alias(libs.plugins.android.library)
    alias(libs.plugins.kotlin.android)
}

android {
    namespace = "org.fcast.sender_sdk.layouts"
    compileSdk = 35

    defaultConfig {
        minSdk = 21
        testOptions.targetSdk = 34
        vectorDrawables.useSupportLibrary = true
    }

    sourceSets {
        getByName("main") {
            res.srcDirs("src/res")
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
    implementation("androidx.recyclerview:recyclerview:1.4.0")
    implementation("androidx.constraintlayout:constraintlayout:2.2.1")
    implementation("com.google.android.material:material:1.12.0")
}

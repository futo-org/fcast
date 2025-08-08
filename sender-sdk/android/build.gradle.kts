import com.android.aaptcompiler.android.isTruthy

plugins {
    alias(libs.plugins.android.library)
    alias(libs.plugins.kotlin.android)
}

android {
    namespace = "org.fcast.sender_sdk"
    compileSdk = 35

    defaultConfig {
        minSdk = 26
        testOptions.targetSdk = 34
    }

    sourceSets {
        getByName("main") {
            java.srcDirs("src")
            jniLibs.srcDir("src/jniLibs")
            if (project.hasProperty("includeResources")
                && project.properties["includeResources"].isTruthy()) {
                res.srcDirs("src/res")
            }
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
    implementation("androidx.recyclerview:recyclerview:1.4.0")
    implementation("androidx.constraintlayout:constraintlayout:2.2.1")
    implementation("com.google.android.material:material:1.12.0")
    implementation(libs.androidx.appcompat)
}

import com.vanniktech.maven.publish.AndroidSingleVariantLibrary

plugins {
    alias(libs.plugins.android.library)
    alias(libs.plugins.kotlin.android)
    id("org.jetbrains.dokka") version "2.0.0"
    id("com.vanniktech.maven.publish") version "0.37.0"
}

android {
    namespace = "org.fcast.sender_sdk"
    compileSdk = 35

    defaultConfig {
        minSdk = 26
        testOptions.targetSdk = 34
        consumerProguardFiles("consumer-rules.pro")
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
    // Runtime scope in the published POM, so consumers get JNA transitively
    implementation("net.java.dev.jna:jna:5.13.0@aar")
}

mavenPublishing {
    publishToMavenCentral(automaticRelease = true)
    signAllPublications()

    val sdkVersion = (project.findProperty("sdkVersion") as String?) ?: "0.4.1"
    coordinates("org.fcast", "sender-sdk", sdkVersion)

    configure(
        AndroidSingleVariantLibrary(
            variant = "release",
            sourcesJar = true,
            publishJavadocJar = true,
        )
    )

    pom {
        name.set("FCast Sender SDK")
        description.set("SDK for creating casting sender applications for the FCast protocol")
        inceptionYear.set("2025")
        url.set("https://gitlab.futo.org/videostreaming/fcast")
        licenses {
            license {
                name.set("MIT License")
                url.set("https://gitlab.futo.org/videostreaming/fcast/-/blob/master/LICENSE")
                distribution.set("repo")
            }
        }
        developers {
            developer {
                id.set("futo")
                name.set("FUTO")
                url.set("https://futo.org")
            }
        }
        scm {
            url.set("https://gitlab.futo.org/videostreaming/fcast")
            connection.set("scm:git:https://gitlab.futo.org/videostreaming/fcast.git")
            developerConnection.set("scm:git:ssh://git@gitlab.futo.org/videostreaming/fcast.git")
        }
    }
}

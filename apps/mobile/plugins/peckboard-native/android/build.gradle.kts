plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    // Not `com.peckboard.native`: `native` is a Java keyword and would break
    // the generated R class.
    namespace = "com.peckboard.nativeplugin"
    // tauri-android's AAR metadata requires 36+ (release builds fail otherwise).
    compileSdk = 36

    defaultConfig {
        minSdk = 24
        consumerProguardFiles("consumer-rules.pro")
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.appcompat:appcompat:1.7.0")
    implementation(project(":tauri-android"))
}

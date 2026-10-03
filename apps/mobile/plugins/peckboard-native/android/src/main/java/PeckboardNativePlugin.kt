package com.peckboard.nativeplugin

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.pm.PackageManager
import android.graphics.Bitmap
import android.net.Uri
import android.os.Build
import android.os.Message
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import android.view.View
import android.webkit.ConsoleMessage
import android.webkit.GeolocationPermissions
import android.webkit.JsPromptResult
import android.webkit.JsResult
import android.webkit.PermissionRequest
import android.webkit.ValueCallback
import android.webkit.WebChromeClient
import android.webkit.WebView
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import androidx.core.content.ContextCompat
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

// Rust core is the only caller (no JS commands are exposed); see src/mobile.rs.

@InvokeArg
class KeyArgs {
    lateinit var key: String
}

@InvokeArg
class SetArgs {
    lateinit var key: String
    lateinit var value: String
}

@InvokeArg
class LifecycleArgs {
    lateinit var channel: Channel
}

@TauriPlugin
class PeckboardNativePlugin(private val activity: Activity) : Plugin(activity) {
    private var lifecycle: Channel? = null
    private val vault by lazy { SecretVault(activity) }

    override fun load(webView: WebView) {
        webView.settings.mediaPlaybackRequiresUserGesture = false
        webView.settings.allowFileAccess = false
        appUserAgent(webView)
        installBack(webView)
        webView.settings.allowContentAccess = false
        // Posted so it runs after wry has installed its own chrome client,
        // which we wrap rather than replace (file chooser, JS dialogs, its
        // runtime-permission flow). getWebChromeClient() needs API 26.
        webView.post {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                val inner = webView.webChromeClient
                if (inner !is LoopbackChromeClient) {
                    webView.webChromeClient = LoopbackChromeClient(activity, inner)
                }
            }
        }
    }
    /**
     * System Back: within a box UI, WebView history (whose first entry is the
     * shell, so it leads back to the box list); on a box page with no history,
     * the shell itself. On the shell, its own screens (`window.__pbmBack`),
     * then the app goes to the background.
     */
    private fun installBack(webView: WebView) {
        val owner = activity as? ComponentActivity ?: return
        owner.onBackPressedDispatcher.addCallback(owner, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                val url = webView.url?.let(Uri::parse)
                val onBox = url?.scheme == "http" && url.host == "127.0.0.1"
                when {
                    onBox && webView.canGoBack() -> webView.goBack()
                    onBox -> webView.loadUrl(shellUrl(webView))
                    else -> webView.evaluateJavascript(
                        "!!(window.__pbmBack && window.__pbmBack())"
                    ) { handled ->
                        if (handled != "true") owner.moveTaskToBack(true)
                    }
                }
            }
        })
    }

    /** The shell's URL: the origin of the first history entry (index.html). */
    private fun shellUrl(webView: WebView): String {
        val first = webView.copyBackForwardList().takeIf { it.size > 0 }
            ?.getItemAtIndex(0)?.url?.let(Uri::parse)
        return if (first != null && first.host == "tauri.localhost") {
            "${first.scheme}://tauri.localhost/"
        } else {
            "http://tauri.localhost/"
        }
    }

    /** ` PeckBoardApp/<version>`: lets the box UI offer "Switch box". */
    private fun appUserAgent(webView: WebView) {
        val version = try {
            activity.packageManager.getPackageInfo(activity.packageName, 0).versionName
        } catch (e: Exception) {
            null
        } ?: "0"
        val ua = webView.settings.userAgentString
        if (!ua.contains("PeckBoardApp/")) {
            webView.settings.userAgentString = "$ua PeckBoardApp/$version"
        }
    }

    override fun onResume() {
        emit("foreground")
    }

    override fun onStop() {
        emit("background")
    }

    private fun emit(state: String) {
        lifecycle?.send(JSObject().put("state", state))
    }

    @Command
    fun watchLifecycle(invoke: Invoke) {
        val args = invoke.parseArgs(LifecycleArgs::class.java)
        lifecycle = args.channel
        invoke.resolve()
    }

    @Command
    fun secretGet(invoke: Invoke) {
        val args = invoke.parseArgs(KeyArgs::class.java)
        try {
            val value = vault.get(args.key)
            val out = JSObject()
            if (value != null) out.put("value", value)
            invoke.resolve(out)
        } catch (e: Exception) {
            invoke.reject("secure storage read failed: ${e.message}")
        }
    }

    @Command
    fun secretSet(invoke: Invoke) {
        val args = invoke.parseArgs(SetArgs::class.java)
        try {
            vault.set(args.key, args.value)
            invoke.resolve()
        } catch (e: Exception) {
            invoke.reject("secure storage write failed: ${e.message}")
        }
    }

    @Command
    fun secretDelete(invoke: Invoke) {
        val args = invoke.parseArgs(KeyArgs::class.java)
        vault.delete(args.key)
        invoke.resolve()
    }
}

/**
 * Values encrypted with an AES-256-GCM key that lives in the Android Keystore
 * (non-exportable), ciphertext in app-private SharedPreferences. Backups are
 * disabled in the manifest; a restored copy would be unreadable anyway since
 * Keystore keys never leave the device.
 */
class SecretVault(context: Context) {
    private val prefs = context.getSharedPreferences("peckboard_secure", Context.MODE_PRIVATE)

    private fun key(): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getKey(ALIAS, null) as? SecretKey)?.let { return it }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(
            KeyGenParameterSpec.Builder(
                ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build()
        )
        return gen.generateKey()
    }

    fun set(name: String, value: String) {
        val cipher = Cipher.getInstance(TRANSFORM)
        cipher.init(Cipher.ENCRYPT_MODE, key())
        cipher.updateAAD(name.toByteArray())
        val ct = cipher.doFinal(value.toByteArray(Charsets.UTF_8))
        val blob = cipher.iv + ct
        prefs.edit().putString(name, Base64.encodeToString(blob, Base64.NO_WRAP)).commit()
    }

    /** Null when absent or undecryptable (e.g. key lost after a restore). */
    fun get(name: String): String? {
        val stored = prefs.getString(name, null) ?: return null
        return try {
            val blob = Base64.decode(stored, Base64.NO_WRAP)
            val cipher = Cipher.getInstance(TRANSFORM)
            cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, blob, 0, IV_LEN))
            cipher.updateAAD(name.toByteArray())
            String(cipher.doFinal(blob, IV_LEN, blob.size - IV_LEN), Charsets.UTF_8)
        } catch (e: Exception) {
            null
        }
    }

    fun delete(name: String) {
        prefs.edit().remove(name).commit()
    }

    companion object {
        private const val ALIAS = "peckboard_pairing_v1"
        private const val TRANSFORM = "AES/GCM/NoPadding"
        private const val IV_LEN = 12
    }
}

/**
 * Wraps wry's chrome client. Microphone capture is granted without a web
 * prompt for the loopback origin (the tunnelled box UI) once the app holds
 * RECORD_AUDIO; otherwise the request goes to wry's client, which asks the
 * OS. Requests from any other origin are denied.
 */
class LoopbackChromeClient(
    private val activity: Activity,
    private val inner: WebChromeClient?,
) : WebChromeClient() {
    override fun onPermissionRequest(request: PermissionRequest) {
        val origin = request.origin
        val loopback = origin.scheme == "http" && origin.host == "127.0.0.1"
        if (!loopback) {
            request.deny()
            return
        }
        val audioOnly = request.resources.all { it == PermissionRequest.RESOURCE_AUDIO_CAPTURE }
        val micGranted = ContextCompat.checkSelfPermission(
            activity, Manifest.permission.RECORD_AUDIO
        ) == PackageManager.PERMISSION_GRANTED
        when {
            audioOnly && micGranted -> request.grant(request.resources)
            inner != null -> inner.onPermissionRequest(request)
            else -> request.deny()
        }
    }

    override fun onPermissionRequestCanceled(request: PermissionRequest) {
        inner?.onPermissionRequestCanceled(request) ?: super.onPermissionRequestCanceled(request)
    }

    override fun onShowFileChooser(
        webView: WebView,
        filePathCallback: ValueCallback<Array<Uri>>,
        fileChooserParams: FileChooserParams,
    ): Boolean =
        inner?.onShowFileChooser(webView, filePathCallback, fileChooserParams)
            ?: super.onShowFileChooser(webView, filePathCallback, fileChooserParams)

    override fun onJsAlert(view: WebView, url: String, message: String, result: JsResult): Boolean =
        inner?.onJsAlert(view, url, message, result) ?: super.onJsAlert(view, url, message, result)

    override fun onJsConfirm(view: WebView, url: String, message: String, result: JsResult): Boolean =
        inner?.onJsConfirm(view, url, message, result)
            ?: super.onJsConfirm(view, url, message, result)

    override fun onJsPrompt(
        view: WebView,
        url: String,
        message: String,
        defaultValue: String?,
        result: JsPromptResult,
    ): Boolean =
        inner?.onJsPrompt(view, url, message, defaultValue, result)
            ?: super.onJsPrompt(view, url, message, defaultValue, result)

    override fun onJsBeforeUnload(view: WebView, url: String, message: String, result: JsResult): Boolean =
        inner?.onJsBeforeUnload(view, url, message, result)
            ?: super.onJsBeforeUnload(view, url, message, result)

    override fun onProgressChanged(view: WebView, newProgress: Int) {
        inner?.onProgressChanged(view, newProgress) ?: super.onProgressChanged(view, newProgress)
    }

    override fun onReceivedTitle(view: WebView, title: String?) {
        inner?.onReceivedTitle(view, title) ?: super.onReceivedTitle(view, title)
    }

    override fun onReceivedIcon(view: WebView, icon: Bitmap?) {
        inner?.onReceivedIcon(view, icon) ?: super.onReceivedIcon(view, icon)
    }

    override fun onConsoleMessage(consoleMessage: ConsoleMessage): Boolean =
        inner?.onConsoleMessage(consoleMessage) ?: super.onConsoleMessage(consoleMessage)

    override fun onGeolocationPermissionsShowPrompt(
        origin: String,
        callback: GeolocationPermissions.Callback,
    ) {
        // The box UI has no use for location.
        callback.invoke(origin, false, false)
    }

    override fun onShowCustomView(view: View, callback: CustomViewCallback) {
        inner?.onShowCustomView(view, callback) ?: super.onShowCustomView(view, callback)
    }

    override fun onHideCustomView() {
        inner?.onHideCustomView() ?: super.onHideCustomView()
    }

    override fun onCreateWindow(
        view: WebView,
        isDialog: Boolean,
        isUserGesture: Boolean,
        resultMsg: Message,
    ): Boolean = inner?.onCreateWindow(view, isDialog, isUserGesture, resultMsg) ?: false

    override fun onCloseWindow(window: WebView) {
        inner?.onCloseWindow(window) ?: super.onCloseWindow(window)
    }

    override fun getDefaultVideoPoster(): Bitmap? =
        inner?.defaultVideoPoster ?: super.getDefaultVideoPoster()
}

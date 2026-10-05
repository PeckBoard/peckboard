package com.peckboard.nativeplugin

import android.Manifest
import android.app.Activity
import android.app.AlertDialog
import android.content.Context
import android.content.pm.PackageManager
import android.graphics.Bitmap
import android.net.ConnectivityManager
import android.net.Network
import android.net.Uri
import android.os.Build
import android.os.Message
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import android.view.View
import android.webkit.ConsoleMessage
import android.webkit.CookieManager
import android.webkit.GeolocationPermissions
import android.webkit.JsPromptResult
import android.webkit.JsResult
import android.webkit.PermissionRequest
import android.webkit.ValueCallback
import android.webkit.WebChromeClient
import android.webkit.WebStorage
import android.webkit.WebView
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import androidx.core.content.ContextCompat
import androidx.webkit.WebStorageCompat
import androidx.webkit.WebViewFeature
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

@InvokeArg
class NetworkArgs {
    lateinit var channel: Channel
}

@InvokeArg
class ClearSiteDataArgs {
    /** `http://127.0.0.1:<port>` of the removed box. */
    lateinit var origin: String
    /** The last box is gone: wipe everything under the host. */
    var hostWide: Boolean = false
}

/** Who may capture the microphone (mirrors the Rust `MicPolicy`). */
class MicPolicy {
    lateinit var origin: String
    lateinit var boxId: String
    lateinit var boxName: String
    /** null: not asked yet. */
    var allowed: Boolean? = null
}

@InvokeArg
class MicPolicyArgs {
    var policy: MicPolicy? = null
}

@InvokeArg
class MicDecisionArgs {
    lateinit var channel: Channel
}

/** `scheme://host[:port]`, like the Rust side's `nav::origin`. */
internal fun originOf(uri: Uri): String? {
    val scheme = uri.scheme ?: return null
    val host = uri.host ?: return null
    return if (uri.port == -1) "$scheme://$host" else "$scheme://$host:${uri.port}"
}

@TauriPlugin
class PeckboardNativePlugin(private val activity: Activity) : Plugin(activity) {
    private var lifecycle: Channel? = null
    @Volatile private var network: Channel? = null
    @Volatile private var micDecisions: Channel? = null
    /** Written by `setMicPolicy` (any thread), read by the chrome client on
     *  the UI thread. Never posted to the UI thread: Rust plugin calls are
     *  dispatched to and awaited from that thread. */
    @Volatile private var micPolicy: MicPolicy? = null
    private var webView: WebView? = null
    private var networkCallback: ConnectivityManager.NetworkCallback? = null
    private val vault by lazy { SecretVault(activity) }

    override fun load(webView: WebView) {
        this.webView = webView
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
                    webView.webChromeClient = LoopbackChromeClient(
                        activity, webView, inner,
                        policy = { micPolicy },
                        decided = { boxId, allowed ->
                            micDecisions?.send(JSObject().put("boxId", boxId).put("allowed", allowed))
                        },
                    )
                }
            }
        }
    }

    /**
     * System Back: within a box UI, WebView history (whose first entry is the
     * shell — the Rust core clears history whenever the shell finishes
     * loading — so it leads back to the box list); on a box page with no
     * history, the shell itself. On the shell, its own screens
     * (`window.__pbmBack`), then the app goes to the background.
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

    /** ` PeckBoardApp/<version>`: tells the box UI it runs inside the app. */
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

    /**
     * Reports a new default network (Wi-Fi <-> cellular, another Wi-Fi) and
     * the loss of the current one, so the tunnel reconnects at once instead
     * of waiting for its pings to time out. The first `onAvailable` is the
     * network at registration and isn't reported. Callbacks run on the
     * ConnectivityManager thread; the Rust side debounces.
     */
    @Command
    fun watchNetwork(invoke: Invoke) {
        val args = invoke.parseArgs(NetworkArgs::class.java)
        network = args.channel
        if (networkCallback == null) {
            val cm = activity.getSystemService(ConnectivityManager::class.java)
            val callback = object : ConnectivityManager.NetworkCallback() {
                private var current: Network? = null
                private var started = false

                override fun onAvailable(n: Network) {
                    val changed = started && n != current
                    started = true
                    current = n
                    if (changed) emitNetwork("available")
                }

                override fun onLost(n: Network) {
                    if (n == current) {
                        current = null
                        emitNetwork("lost")
                    }
                }
            }
            try {
                cm.registerDefaultNetworkCallback(callback)
                networkCallback = callback
            } catch (e: Exception) {
                invoke.reject("network monitor failed: ${e.message}")
                return
            }
        }
        invoke.resolve()
    }

    private fun emitNetwork(detail: String) {
        network?.send(JSObject().put("detail", detail))
    }

    /** The active box's microphone policy (null denies everything). */
    @Command
    fun setMicPolicy(invoke: Invoke) {
        val args = invoke.parseArgs(MicPolicyArgs::class.java)
        micPolicy = args.policy
        invoke.resolve()
    }

    @Command
    fun watchMicDecision(invoke: Invoke) {
        val args = invoke.parseArgs(MicDecisionArgs::class.java)
        micDecisions = args.channel
        invoke.resolve()
    }

    /**
     * Drop the back/forward list (all but the current page). History
     * navigations don't pass `shouldOverrideUrlLoading`, so without this
     * Back from a new box could land on another box's retired origin.
     */
    @Command
    fun clearHistory(invoke: Invoke) {
        val wv = webView
        activity.runOnUiThread { wv?.clearHistory() }
        invoke.resolve()
    }

    /**
     * Website data of a removed box. `WebStorage.deleteOrigin` is exact to
     * the origin (port included) but only covers IndexedDB / file system /
     * Web SQL — not localStorage, where the box UI keeps its login. So with
     * `hostWide` (no box left) everything under 127.0.0.1 goes too:
     * `WebStorageCompat.deleteBrowsingDataForSite` (all storage, cookies,
     * caches, service workers) where the installed WebView has it, else
     * `deleteAllData` + all cookies + the HTTP cache. Data a per-origin
     * clear leaves behind stays orphaned behind a port the app never hands
     * out again.
     */
    @Command
    fun clearSiteData(invoke: Invoke) {
        val args = invoke.parseArgs(ClearSiteDataArgs::class.java)
        val wv = webView
        activity.runOnUiThread {
            try {
                val storage = WebStorage.getInstance()
                storage.deleteOrigin(args.origin)
                if (args.hostWide) {
                    val host = Uri.parse(args.origin).host ?: "127.0.0.1"
                    if (WebViewFeature.isFeatureSupported(WebViewFeature.DELETE_BROWSING_DATA)) {
                        WebStorageCompat.deleteBrowsingDataForSite(
                            storage, host, ContextCompat.getMainExecutor(activity)
                        ) {}
                    } else {
                        storage.deleteAllData()
                    }
                    val cookies = CookieManager.getInstance()
                    cookies.removeAllCookies(null)
                    cookies.flush()
                    wv?.clearCache(true)
                }
                invoke.resolve()
            } catch (e: Exception) {
                invoke.reject("clearing website data failed: ${e.message}")
            }
        }
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
 * Wraps wry's chrome client. Microphone capture is granted only to the
 * active box's loopback origin (`policy().origin`) — the requesting frame's
 * origin and the page's own origin must both match, which rules out
 * sub-frames from elsewhere — and only after the user allowed it for that
 * box: the first request shows "Allow <box> to use the microphone?" and the
 * answer is remembered with the box (`decided`). Granting still goes through
 * wry's client when the app doesn't hold RECORD_AUDIO yet, so the OS asks
 * once. Camera, other origins and no active box are denied.
 */
class LoopbackChromeClient(
    private val activity: Activity,
    private val webView: WebView,
    private val inner: WebChromeClient?,
    private val policy: () -> MicPolicy?,
    private val decided: (String, Boolean) -> Unit,
) : WebChromeClient() {
    private var prompt: AlertDialog? = null

    override fun onPermissionRequest(request: PermissionRequest) {
        val policy = policy()
        val resources = request.resources
        val audioOnly = resources.isNotEmpty() &&
            resources.all { it == PermissionRequest.RESOURCE_AUDIO_CAPTURE }
        val pageOrigin = webView.url?.let(Uri::parse)?.let(::originOf)
        if (policy == null || !audioOnly ||
            originOf(request.origin) != policy.origin || pageOrigin != policy.origin
        ) {
            request.deny()
            return
        }
        when (policy.allowed) {
            true -> grantMic(request)
            false -> request.deny()
            null -> ask(request, policy)
        }
    }

    /** One prompt per box; a dismissed prompt denies without remembering. */
    private fun ask(request: PermissionRequest, policy: MicPolicy) {
        prompt?.dismiss()
        prompt = AlertDialog.Builder(activity)
            .setTitle("Microphone")
            .setMessage("Allow “${policy.boxName}” to use the microphone?")
            .setPositiveButton("Allow") { _, _ ->
                decided(policy.boxId, true)
                grantMic(request)
            }
            .setNegativeButton("Don’t allow") { _, _ ->
                decided(policy.boxId, false)
                request.deny()
            }
            .setOnCancelListener { request.deny() }
            .setOnDismissListener { prompt = null }
            .show()
    }

    private fun grantMic(request: PermissionRequest) {
        val held = ContextCompat.checkSelfPermission(
            activity, Manifest.permission.RECORD_AUDIO
        ) == PackageManager.PERMISSION_GRANTED
        when {
            held -> request.grant(request.resources)
            // wry asks the OS for RECORD_AUDIO, then grants.
            inner != null -> inner.onPermissionRequest(request)
            else -> request.deny()
        }
    }

    override fun onPermissionRequestCanceled(request: PermissionRequest) {
        prompt?.dismiss()
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

package io.zxf.flowsplice.pty

import android.app.Activity
import android.os.Bundle
import android.os.Build
import android.provider.Settings
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.webkit.JavascriptInterface
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.RenderProcessGoneDetail
import android.webkit.WebViewClient
import android.widget.TextView
import org.json.JSONObject
import org.json.JSONArray
import java.io.ByteArrayInputStream
import java.util.UUID

class MainActivity : Activity() {
    private val handler = Handler(Looper.getMainLooper())
    private var web: WebView? = null
    private data class Home(val id: String, val info: JSONObject, var handle: Long = 0L, var error: String? = null, var pendingPassword: String? = null, var openOptions: String? = null, var polling: Boolean = true, var actionUntil: Long = 0L, var recovered: Boolean = false, var disconnectStarted: Long? = null)
    private val homes = linkedMapOf<String, Home>()
    private var classMode = false
    private var foreground = false
    private var ready = false
    private var rendering = false
    private var generation = 0
    private var batchToken = 0
    private var resumePending = false
    private val disconnecting = mutableSetOf<String>()
    private val disconnectRequested = mutableSetOf<String>()
    private var inFlightBytes = 0
    private val snapshots = linkedMapOf<String, JSONObject>()
    private data class Retained(val homes: List<Home>, val classMode: Boolean, val snapshots: Map<String, JSONObject>, val disconnecting: Set<String>, val requested: Set<String>)
    private val queued = java.util.ArrayDeque<BridgeEvent>()
    private var queuedBytes = 0
    private var backgroundExpired = false
    private val expiry = Runnable { backgroundExpired = true; suspendTransport() }
    private val renderTimeout = Runnable { recoverRenderer() }

    private fun suspendTransport() {
        if (disconnecting.isNotEmpty()) return
        disconnecting.addAll(homes.values.filter { it.handle != 0L }.map { it.id })
        disconnectRequested.clear()
        homes.values.forEach {
            it.pendingPassword = null
            if (it.id in disconnecting && it.disconnectStarted == null) it.disconnectStarted = SystemClock.elapsedRealtime()
        }
        // Never deliver pre-reset output or manufacture a disconnected acknowledgement.
        queued.clear(); queuedBytes = 0
        if (ready) dispatch(JSONObject().put("type", "lifecycle").put("active", false))
        resumePending = foreground
        startPolling()
    }

    private fun recoverRenderer() {
        if (!foreground) backgroundExpired = true
        suspendTransport()
        ready = false; rendering = false; inFlightBytes = 0; generation++
        handler.removeCallbacks(renderTimeout)
        queued.clear(); queuedBytes = 0
        web?.removeJavascriptInterface("FlowSpliceNative"); web?.destroy(); web = null
        if (foreground) createWebView()
    }
    private val origin = "https://appassets.androidplatform.net"
    private val page get() = "$origin/assets/pty/index.html"

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val retained = lastNonConfigurationInstance as? Retained
        if (retained != null) {
            classMode = retained.classMode
            retained.homes.forEach { homes[it.id] = it }
            snapshots.putAll(retained.snapshots)
            disconnecting.addAll(retained.disconnecting)
            disconnectRequested.addAll(retained.requested)
            createWebView()
            return
        }
        try {
            val root = assets.open("bootstrap/deployment-root.pub").bufferedReader().use { it.readText() }.trim()
            val configurations = assets.list("bootstrap").orEmpty().filter { it in listOf("service-class.json", "homes.json", "business.json") }
            require(configurations.size == 1)
            classMode = configurations.single() == "service-class.json"
            val entries = if (classMode) {
                listOf(JSONObject().put("id", "service-class").put("name", "PTY").put("platform", "host")
                    .put("descriptor", JSONObject(assets.open("bootstrap/service-class.json").bufferedReader().use { it.readText() })))
            } else if (configurations.single() == "homes.json") {
                parseHomes(JSONObject(assets.open("bootstrap/homes.json").bufferedReader().use { it.readText() }))
            } else listOf(JSONObject().put("id", "default").put("name", "我的 Mac").put("platform", "macos")
                .put("descriptor", JSONObject(assets.open("bootstrap/business.json").bufferedReader().use { it.readText() })))
            assets.open("pty/index.html").close()
            for (entry in entries) {
                val id = entry.getString("id")
                val home = Home(id, JSONObject().put("id", id).put("name", entry.getString("name"))
                    .put("platform", entry.getString("platform")).put("relay", entry.optString("relay")))
                homes[id] = home
                try {
                    val prefs = getSharedPreferences(if (classMode) "pty-identity-service-class" else if (id == "default") "pty-identity" else "pty-identity-home-$id", MODE_PRIVATE)
                    val identity = prefs.getString("travel-id", null) ?: UUID.randomUUID().toString().also {
                        check(prefs.edit().putString("travel-id", it).commit())
                    }
                    val directory = if (classMode) filesDir.resolve("service-class") else if (id == "default") filesDir.resolve("installation") else filesDir.resolve("installation/homes/$id")
                    val options = JSONObject().put("install_dir", directory.absolutePath).put("root_public_key", root)
                        .put(if (classMode) "service_class" else "descriptor", entry.getJSONObject("descriptor")).put("travel_id", identity).put("label", deviceLabel(runCatching { Settings.Global.getString(contentResolver, Settings.Global.DEVICE_NAME) }.getOrNull(), Build.MODEL))
                    home.openOptions = options.toString()
                    val opened = nativeResponse { NativePty.open(home.openOptions!!) }
                    check(opened.optBoolean("ok"))
                    home.handle = opened.getJSONObject("data").getLong("handle")
                } catch (_: Exception) { home.error = "此 Home 的私有配置无法初始化" }
            }
            createWebView()
        } catch (_: Exception) {
            homes.values.forEach { if (it.handle != 0L) runCatching { NativePty.close(it.handle) } }; homes.clear()
            setContentView(TextView(this).apply { text = "FlowSplice PTY 尚未配置。请安装包含私有部署信任、业务描述和终端资源的安装包。"; setPadding(24, 48, 24, 24) })
        }
    }

    private fun createWebView() {
        val currentGeneration = ++generation
        val view = WebView(this)
        web = view
        with(view.settings) {
            javaScriptEnabled = true
            allowFileAccess = false
            allowContentAccess = false
            @Suppress("DEPRECATION")
            allowFileAccessFromFileURLs = false
            @Suppress("DEPRECATION")
            allowUniversalAccessFromFileURLs = false
            mixedContentMode = android.webkit.WebSettings.MIXED_CONTENT_NEVER_ALLOW
            setSupportMultipleWindows(false)
            javaScriptCanOpenWindowsAutomatically = false
            domStorageEnabled = false
            @Suppress("DEPRECATION")
            saveFormData = false
        }
        view.addJavascriptInterface(object {
            @JavascriptInterface fun rendered(token: Int) { handler.post {
                if (web === view && generation == currentGeneration && token == batchToken) {
                    rendering = false; inFlightBytes = 0; handler.removeCallbacks(renderTimeout); flushEvents()
                }
            } }
            @JavascriptInterface fun renderFailed(token: Int) { handler.post {
                if (web === view && generation == currentGeneration && token == batchToken) recoverRenderer()
            } }
            @JavascriptInterface fun send(json: String) {
                handler.post { if (web === view && generation == currentGeneration && view.url == page) action(json) }
            }
        }, "FlowSpliceNative")
        view.webViewClient = object : WebViewClient() {
            override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean = true
            override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse {
                val uri = request.url
                val path = uri.encodedPath.orEmpty()
                if (uri.scheme != "https" || uri.host != "appassets.androidplatform.net" || uri.port != -1 || uri.encodedUserInfo != null || request.method != "GET" || !path.startsWith("/assets/pty/") || path.contains("..") || path.contains('\\') || path.contains('%') || uri.query != null || uri.fragment != null) return blocked()
                val relative = path.removePrefix("/assets/")
                return try {
                    val mime = when { path.endsWith(".html") -> "text/html"; path.endsWith(".js") -> "application/javascript"; path.endsWith(".css") -> "text/css"; path.endsWith(".woff2") -> "font/woff2"; else -> return blocked() }
                    WebResourceResponse(mime, "UTF-8", 200, "OK", mapOf("Content-Security-Policy" to "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; font-src 'self'; img-src 'self' data:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'", "Cache-Control" to "no-store"), assets.open(relative))
                } catch (_: Exception) { blocked() }
            }
            override fun onRenderProcessGone(view: WebView, detail: RenderProcessGoneDetail): Boolean {
                if (web === view) recoverRenderer()
                return true
            }
            override fun onPageFinished(view: WebView, url: String) {
                if (url != page) ready = false
            }
        }
        view.setDownloadListener { _, _, _, _, _ -> }
        setContentView(view)
        view.loadUrl(page)
    }

    private fun blocked() = WebResourceResponse("text/plain", "UTF-8", 403, "Blocked", emptyMap(), ByteArrayInputStream(ByteArray(0)))
    private fun dispatch(event: JSONObject) {
        web?.evaluateJavascript("window.flowsplice.receive(JSON.parse(${JSONObject.quote(event.toString())}));", null)
    }
    private fun action(json: String) {
        if (json.toByteArray(Charsets.UTF_8).size > 128 * 1024) return
        var home: Home? = null
        var action: JSONObject? = null
        try {
            action = JSONObject(json)
            if (action.optString("op") == "save_workspace" && action.length() == 2) {
                WorkspaceStore.save(this, classMode, action.getJSONObject("value")); return
            }
            if (action.optString("op") == "ready" && action.length() == 1) {
                ready = true
                resumePending = foreground
                dispatch(JSONObject().put("type", "workspace").put("value", WorkspaceStore.load(this, classMode) ?: JSONObject.NULL))
                dispatch(JSONObject().put("type", "lifecycle").put("active", false))
                dispatch(JSONObject().put("type", "platform").put("platform", "android"))
                if (!classMode) dispatch(JSONObject().put("type", "homes").put("platform", "android").put("homes", JSONArray(homes.values.map { it.info })))
                if (disconnecting.isEmpty()) snapshots.values.forEach { dispatch(it) }
                homes.values.forEach { h -> h.error?.let { dispatch(JSONObject().put("type", "error").put("home_id", if (classMode) JSONObject.NULL else h.id).put("message", it)) } }
                handler.removeCallbacks(poll); handler.post(poll)
                return
            }
            if (!ready || !foreground || resumePending) return
            home = if (classMode) homes.values.firstOrNull() else homes[action.optString("home_id")]
            if (home == null) return
            if (home.handle == 0L) {
                dispatch(JSONObject().put("type", "error").put("home_id", if (classMode) action.opt("home_id") ?: JSONObject.NULL else home.id).put("input_id", action.opt("input_id") ?: JSONObject.NULL).put("message", home.error ?: "请重新打开应用以恢复本机终端。")); return
            }
            if (!classMode) action.remove("home_id")
            if (action.optString("op") in listOf("enroll", "connect")) {
                val supplied = action.optString("password")
                val password = supplied.ifEmpty { PasswordStore.load(this, home.id, classMode).orEmpty() }
                if (password.isEmpty()) {
                    dispatch(JSONObject().put("type", "error").put("home_id", if (classMode) JSONObject.NULL else home.id).put("code", "credential_required").put("message", if (classMode) "缺少已保存的私钥密码，请输入密码。" else "此 Home 缺少已保存的私钥密码，请输入密码。")); return
                }
                action.put("password", password)
                if (action.optString("op") == "enroll") PasswordStore.save(this, password, home.id, classMode)
                else if (supplied.isNotEmpty()) home.pendingPassword = supplied
            }
            val result = nativeResponse { NativePty.send(home.handle, action.toString()) }
            if (!result.optBoolean("ok")) {
                home.pendingPassword = null
                dispatch(JSONObject().put("type", "error").put("home_id", if (classMode) action.opt("home_id") ?: JSONObject.NULL else home.id).put("input_id", action.opt("input_id") ?: JSONObject.NULL).put("message", result.optString("error")))
            }
            if (result.optBoolean("ok")) { home.polling = true; home.actionUntil = SystemClock.elapsedRealtime() + 10_000; startPolling() }
            if (action.optString("op") == "disconnect") home.pendingPassword = null
        } catch (_: Exception) {
            home?.pendingPassword = null
            dispatch(JSONObject().put("type", "error").put("home_id", if (classMode) action?.opt("home_id") ?: JSONObject.NULL else home?.id ?: JSONObject.NULL).put("input_id", action?.opt("input_id") ?: JSONObject.NULL).put("message", "本机操作失败"))
        }
    }
    // JNI can throw linkage/native errors; confine Throwable handling to this boundary.
    private fun nativeResponse(call: () -> String): JSONObject {
        val json = try { call() } catch (failure: Throwable) { throw IllegalStateException("Native operation failed", failure) }
        return JSONObject(json)
    }
    private fun startPolling() {
        handler.removeCallbacks(poll)
        if (homes.values.any { NativeLifecycle.shouldPoll(it.handle, it.polling, it.id in disconnecting, it.actionUntil, SystemClock.elapsedRealtime()) }) handler.post(poll)
    }
    private fun recoverHandle(home: Home, batch: JSONArray, reason: String) {
        home.pendingPassword = null; home.polling = false; home.actionUntil = 0L
        val closed = runCatching { nativeResponse { NativePty.close(home.handle) }.optBoolean("ok") }.getOrDefault(false)
        home.handle = 0L
        var message = reason
        if (!closed) message += "; native close failed"
        if (NativeLifecycle.shouldReopen(closed, home.recovered, home.openOptions != null)) {
            home.recovered = true
            try {
                val opened = nativeResponse { NativePty.open(home.openOptions!!) }
                check(opened.optBoolean("ok"))
                home.handle = opened.getJSONObject("data").getLong("handle")
                check(home.handle != 0L)
                home.polling = true
            } catch (_: Exception) { home.handle = 0L; message += "; native reopen failed" }
        }
        disconnecting.remove(home.id); disconnectRequested.remove(home.id); home.disconnectStarted = null
        if (closed) {
            val type = if (classMode) "identity" else "state"
            val key = type + ":" + (if (classMode) "" else home.id)
            val state = snapshots[key]?.let { JSONObject(it.toString()) } ?: JSONObject()
            state.put("type", type).put("connected", false).put("busy", false)
            if (!classMode) state.put("home_id", home.id)
            batch.put(state)
            if (classMode) batch.put(JSONObject().put("type", "homes").put("homes", JSONArray()))
        }
        if (home.handle == 0L) message += "。请重新打开应用以恢复本机终端。"
        home.error = if (home.handle == 0L) message else null
        batch.put(JSONObject().put("type", "error").put("home_id", if (classMode) JSONObject.NULL else home.id).put("message", message))
    }
    private val poll = object : Runnable {
        override fun run() {
            if (homes.isEmpty()) return
            val batch = JSONArray()
            // One shared class queue or up to eight legacy queues; one awaited JS batch.
            for (home in homes.values.filter { NativeLifecycle.shouldPoll(it.handle, it.polling, it.id in disconnecting, it.actionUntil, SystemClock.elapsedRealtime()) }) {
                try {
                    check(!NativeLifecycle.drainExpired(home.disconnectStarted, SystemClock.elapsedRealtime())) { "Native disconnect timed out" }
                    val result = nativeResponse { NativePty.poll(home.handle) }
                    check(result.optBoolean("ok"))
                    val events = result.getJSONArray("data")
                    if (home.id in disconnecting && home.id !in disconnectRequested) {
                        // Discard the drained old events and enqueue the disconnect now;
                        // continuous output must not starve this lifecycle operation.
                        val sent = nativeResponse { NativePty.send(home.handle, "{\"op\":\"disconnect\"}") }
                        if (sent.optBoolean("ok")) disconnectRequested.add(home.id)
                        continue
                    }
                    for (index in 0 until events.length()) {
                        val event = events.getJSONObject(index)
                        if (event.optString("type") == (if (classMode) "identity" else "state")) {
                            home.polling = event.optBoolean("connected") || event.optBoolean("busy")
                            if (event.optBoolean("connected") && !event.optBoolean("busy")) home.recovered = false
                        }
                        if (home.id in disconnecting) {
                            val terminal = event.optString("type") == (if (classMode) "identity" else "state") && !event.optBoolean("connected") && !event.optBoolean("busy")
                            if (!terminal) continue
                            disconnecting.remove(home.id); disconnectRequested.remove(home.id); home.disconnectStarted = null
                        }
                        if (event.optString("type") == (if (classMode) "identity" else "state") && event.optBoolean("connected")) {
                            try { home.pendingPassword?.let { PasswordStore.save(this@MainActivity, it, home.id, classMode) } }
                            catch (_: Exception) { batch.put(JSONObject().put("type", "error").put("home_id", if (classMode) JSONObject.NULL else home.id).put("message", "私钥密码保存失败")) }
                            home.pendingPassword = null
                        }
                        if (event.optString("type") == "error") home.pendingPassword = null
                        batch.put(if (classMode) event else event.put("home_id", home.id))
                    }
                } catch (_: Exception) {
                    recoverHandle(home, batch, "本机事件读取或断开失败")
                }
            }
            for (index in 0 until batch.length()) {
                val event = batch.getJSONObject(index)
                val serialized = event.toString()
                if (event.optString("type") in listOf("identity", "homes", "state")) snapshots[event.optString("type") + ":" + event.optString("home_id")] = JSONObject(serialized)
                val fragment = BridgeEvent(serialized)
                if (bridgeBudget(queuedBytes.toLong() + fragment.bytes, queued.size + 1, inFlightBytes) > BRIDGE_LIMIT) {
                    recoverRenderer()
                    break
                }
                queued.add(fragment); queuedBytes += fragment.bytes
            }
            flushEvents()
            handler.removeCallbacks(this)
            if ((foreground || !backgroundExpired || disconnecting.isNotEmpty()) && homes.values.any { NativeLifecycle.shouldPoll(it.handle, it.polling, it.id in disconnecting, it.actionUntil, SystemClock.elapsedRealtime()) }) handler.postDelayed(this, 25)
        }
    }
    internal class BridgeEvent(serialized: String) {
        // JSONObject owns escaping; retain only the interior of its string literal.
        val escaped = JSONObject.quote(serialized).let { it.substring(1, it.length - 1) }
        // Reserve one comma per event (the final event's comma is conservative).
        val bytes = escaped.toByteArray(Charsets.UTF_8).size + 1
    }
    companion object {
        internal const val BRIDGE_LIMIT = 2 * 1024 * 1024
        internal const val BRIDGE_BATCH = 64
        internal fun bridgeLiteral(events: List<BridgeEvent>): String =
            events.joinToString(",", "\"[", "]\"") { it.escaped }
        internal fun bridgeScript(events: List<BridgeEvent>, token: Int): String =
            "window.flowsplice.receiveBatch(JSON.parse(${bridgeLiteral(events)})).then(() => window.FlowSpliceNative.rendered($token), () => window.FlowSpliceNative.renderFailed($token));"
        // Includes quotes, brackets, and both longest possible signed Int tokens.
        internal val bridgeOverhead = bridgeScript(emptyList(), Int.MIN_VALUE).toByteArray(Charsets.UTF_8).size
        internal fun bridgeBudget(fragmentBytes: Long, count: Int, inFlight: Int): Long =
            fragmentBytes + ((count.toLong() + BRIDGE_BATCH - 1) / BRIDGE_BATCH) * bridgeOverhead + inFlight

        internal fun deviceLabel(name: String?, model: String?): String {
            fun clean(value: String?): String = buildString {
                value.orEmpty().codePoints().forEach { point ->
                    if (!Character.isISOControl(point) && point !in setOf(0x061C, 0x200B, 0x200E, 0x200F, 0xFEFF) && point !in 0x202A..0x202E && point !in 0x2060..0x2069) appendCodePoint(point)
                }
            }.trim()
            val base = clean(name).ifEmpty { clean(model).ifEmpty { "Android" } }
            val suffix = " · PTY"
            val result = StringBuilder()
            var bytes = 0
            for (point in base.codePoints().toArray()) {
                val part = String(Character.toChars(point))
                val size = part.toByteArray(Charsets.UTF_8).size
                if (bytes + size + suffix.toByteArray(Charsets.UTF_8).size > 64) break
                result.append(part); bytes += size
            }
            return result.toString().trimEnd() + suffix
        }

        internal fun parseHomes(catalog: JSONObject): List<JSONObject> {
            require(catalog.get("version") == 1)
            val array = catalog.getJSONArray("homes")
            require(array.length() in 1..8)
            val ids = mutableSetOf<String>()
            return (0 until array.length()).map { index ->
                array.getJSONObject(index).also {
                    val id = it.getString("id")
                    val name = it.getString("name")
                    require(Regex("[a-z0-9][a-z0-9_-]{0,47}").matches(id) && ids.add(id))
                    require(name.isNotBlank() && name.toByteArray(Charsets.UTF_8).size <= 128 && name.none { c -> Character.isISOControl(c) })
                    require(it.getString("platform") in listOf("linux", "macos"))
                    require(!it.has("relay") || it.get("relay") is String)
                    require(it.optString("relay").toByteArray(Charsets.UTF_8).size <= 512)
                    it.getJSONObject("descriptor")
                }
            }
        }
    }
    private fun flushEvents() {
        if (!foreground || !ready || rendering) return
        if (queued.isNotEmpty()) {
            val events = ArrayList<BridgeEvent>()
            repeat(minOf(BRIDGE_BATCH, queued.size)) {
                val event = queued.removeFirst()
                queuedBytes -= event.bytes
                events.add(event)
            }
            rendering = true
            val token = ++batchToken
            val script = bridgeScript(events, token)
            inFlightBytes = script.toByteArray(Charsets.UTF_8).size
            web?.evaluateJavascript(script, null)
            handler.removeCallbacks(renderTimeout); handler.postDelayed(renderTimeout, 10_000)
        } else if (resumePending && disconnecting.isEmpty()) {
            resumePending = false
            dispatch(JSONObject().put("type", "lifecycle").put("active", true))
        }
    }
    override fun onResume() {
        super.onResume(); foreground = true; resumePending = true; backgroundExpired = false
        handler.removeCallbacks(expiry)
        if (rendering) handler.postDelayed(renderTimeout, 10_000)
        if (web == null && homes.isNotEmpty()) createWebView()
        handler.removeCallbacks(poll); handler.post(poll)
    }
    override fun onStop() {
        foreground = false; resumePending = false
        handler.removeCallbacks(renderTimeout)
        if (ready) dispatch(JSONObject().put("type", "lifecycle").put("active", false))
        handler.removeCallbacks(expiry); handler.postDelayed(expiry, 30_000)
        super.onStop()
    }
    override fun onRetainNonConfigurationInstance(): Any {
        suspendTransport()
        return Retained(homes.values.toList(), classMode, snapshots.toMap(), disconnecting.toSet(), disconnectRequested.toSet())
    }
    override fun onDestroy() {
        foreground = false; ready = false; generation++
        handler.removeCallbacksAndMessages(null)
        if (!isChangingConfigurations) homes.values.forEach { if (it.handle != 0L) runCatching { NativePty.close(it.handle) } }
        homes.clear()
        queued.clear(); queuedBytes = 0
        web?.removeJavascriptInterface("FlowSpliceNative"); web?.destroy(); web = null
        super.onDestroy()
    }
}

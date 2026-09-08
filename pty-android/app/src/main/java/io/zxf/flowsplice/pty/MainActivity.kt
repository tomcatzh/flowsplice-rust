package io.zxf.flowsplice.pty

import android.app.Activity
import android.os.Bundle
import android.os.Build
import android.provider.Settings
import android.os.Handler
import android.os.Looper
import android.webkit.JavascriptInterface
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.TextView
import org.json.JSONObject
import org.json.JSONArray
import java.io.ByteArrayInputStream
import java.util.UUID

class MainActivity : Activity() {
    private val handler = Handler(Looper.getMainLooper())
    private var web: WebView? = null
    private data class Home(val id: String, val info: JSONObject, var handle: Long = 0L, var error: String? = null, var pendingPassword: String? = null)
    private val homes = linkedMapOf<String, Home>()
    private var classMode = false
    private var foreground = false
    private var ready = false
    private var rendering = false
    private val origin = "https://appassets.androidplatform.net"
    private val page get() = "$origin/assets/pty/index.html"

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
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
                    val opened = JSONObject(NativePty.open(options.toString()))
                    check(opened.optBoolean("ok"))
                    home.handle = opened.getJSONObject("data").getLong("handle")
                } catch (_: Exception) { home.error = "此 Home 的私有配置无法初始化" }
            }
            createWebView()
        } catch (_: Throwable) {
            homes.values.forEach { if (it.handle != 0L) NativePty.close(it.handle) }; homes.clear()
            setContentView(TextView(this).apply { text = "FlowSplice PTY 尚未配置。请安装包含私有部署信任、业务描述和终端资源的安装包。"; setPadding(24, 48, 24, 24) })
        }
    }

    private fun createWebView() {
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
        }
        view.addJavascriptInterface(object {
            @JavascriptInterface fun rendered() { handler.post { if (view.url == page) rendering = false } }
            @JavascriptInterface fun send(json: String) {
                handler.post { if (foreground && view.url == page) action(json) }
            }
        }, "FlowSpliceNative")
        view.webViewClient = object : WebViewClient() {
            override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean = true
            override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse {
                val uri = request.url
                val path = uri.encodedPath.orEmpty()
                if (uri.scheme != "https" || uri.host != "appassets.androidplatform.net" || uri.port != -1 || request.method != "GET" || !path.startsWith("/assets/pty/") || path.contains("..") || path.contains('%') || uri.query != null || uri.fragment != null) return blocked()
                val relative = path.removePrefix("/assets/")
                return try {
                    val mime = when { path.endsWith(".html") -> "text/html"; path.endsWith(".js") -> "application/javascript"; path.endsWith(".css") -> "text/css"; path.endsWith(".woff2") -> "font/woff2"; else -> return blocked() }
                    WebResourceResponse(mime, "UTF-8", 200, "OK", mapOf("Content-Security-Policy" to "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; font-src 'self'; img-src 'self' data:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'", "Cache-Control" to "no-store"), assets.open(relative))
                } catch (_: Exception) { blocked() }
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
        try {
            val action = JSONObject(json)
            if (action.optString("op") == "ready" && action.length() == 1) {
                ready = true
                dispatch(JSONObject().put("type", "platform").put("platform", "android"))
                if (!classMode) dispatch(JSONObject().put("type", "homes").put("platform", "android").put("homes", JSONArray(homes.values.map { it.info })))
                homes.values.forEach { h -> h.error?.let { dispatch(JSONObject().put("type", "error").put("home_id", if (classMode) JSONObject.NULL else h.id).put("message", it)) } }
                handler.removeCallbacks(poll); handler.post(poll)
                return
            }
            if (!ready) return
            home = if (classMode) homes.values.firstOrNull() else homes[action.optString("home_id")]
            if (home == null) return
            if (home.handle == 0L) return
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
            val result = JSONObject(NativePty.send(home.handle, action.toString()))
            if (!result.optBoolean("ok")) {
                home.pendingPassword = null
                dispatch(JSONObject().put("type", "error").put("home_id", if (classMode) action.opt("home_id") ?: JSONObject.NULL else home.id).put("message", result.optString("error")))
            }
            if (action.optString("op") == "disconnect") home.pendingPassword = null
        } catch (_: Exception) {
            home?.pendingPassword = null
            dispatch(JSONObject().put("type", "error").put("home_id", if (classMode) JSONObject.NULL else home?.id ?: JSONObject.NULL).put("message", "本机操作失败"))
        }
    }
    private val poll = object : Runnable {
        override fun run() {
            if (!foreground || !ready) return
            if (rendering) { handler.postDelayed(this, 25); return }
            val batch = JSONArray()
            // One shared class queue or up to eight legacy queues; one awaited JS batch.
            for (home in homes.values.filter { it.handle != 0L }) {
                try {
                    val result = JSONObject(NativePty.poll(home.handle))
                    check(result.optBoolean("ok"))
                    val events = result.getJSONArray("data")
                    for (index in 0 until events.length()) {
                        val event = events.getJSONObject(index)
                        if (event.optString("type") == (if (classMode) "identity" else "state") && event.optBoolean("connected")) {
                            home.pendingPassword?.let { PasswordStore.save(this@MainActivity, it, home.id, classMode) }; home.pendingPassword = null
                        }
                        if (event.optString("type") == "error") home.pendingPassword = null
                        batch.put(if (classMode) event else event.put("home_id", home.id))
                    }
                } catch (_: Exception) {
                    home.pendingPassword = null
                    batch.put(JSONObject().put("type", "error").put("home_id", if (classMode) JSONObject.NULL else home.id).put("message", "本机事件读取失败"))
                }
            }
            if (batch.length() > 0) {
                rendering = true
                web?.evaluateJavascript("window.flowsplice.receiveBatch(JSON.parse(${JSONObject.quote(batch.toString())})).finally(() => window.FlowSpliceNative.rendered());", null)
            }
            handler.postDelayed(this, 25)
        }
    }
    companion object {
        internal fun deviceLabel(name: String?, model: String?): String {
            fun clean(value: String?): String = buildString {
                value.orEmpty().codePoints().forEach { point ->
                    if (!Character.isISOControl(point) && point !in 0x202A..0x202E && point !in 0x2066..0x2069) appendCodePoint(point)
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
    override fun onResume() { super.onResume(); foreground = true; if (ready) handler.post(poll) }
    override fun onStop() {
        foreground = false
        homes.values.forEach { if (it.handle != 0L) NativePty.send(it.handle, "{\"op\":\"disconnect\"}"); it.pendingPassword = null }
        handler.removeCallbacks(poll)
        super.onStop()
    }
    override fun onDestroy() {
        foreground = false; ready = false; handler.removeCallbacks(poll)
        homes.values.forEach { if (it.handle != 0L) NativePty.close(it.handle) }; homes.clear()
        web?.removeJavascriptInterface("FlowSpliceNative"); web?.destroy(); web = null
        super.onDestroy()
    }
}

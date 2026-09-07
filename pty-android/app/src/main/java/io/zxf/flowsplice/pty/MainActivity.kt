package io.zxf.flowsplice.pty

import android.app.Activity
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.webkit.JavascriptInterface
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.TextView
import org.json.JSONObject
import java.io.ByteArrayInputStream
import java.util.UUID

class MainActivity : Activity() {
    private val handler = Handler(Looper.getMainLooper())
    private var web: WebView? = null
    private var handle = 0L
    private var foreground = false
    private var ready = false
    private var rendering = false
    private var pendingPassword: String? = null
    private val origin = "https://appassets.androidplatform.net"
    private val page get() = "$origin/assets/pty/index.html"

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        try {
            val root = assets.open("bootstrap/deployment-root.pub").bufferedReader().use { it.readText() }.trim()
            val descriptor = JSONObject(assets.open("bootstrap/business.json").bufferedReader().use { it.readText() })
            assets.open("pty/index.html").close()
            val prefs = getSharedPreferences("pty-identity", MODE_PRIVATE)
            val id = prefs.getString("travel-id", null) ?: UUID.randomUUID().toString().also {
                check(prefs.edit().putString("travel-id", it).commit())
            }
            val options = JSONObject().put("install_dir", filesDir.resolve("installation").absolutePath)
                .put("root_public_key", root).put("descriptor", descriptor).put("travel_id", id).put("label", "Android")
            val opened = JSONObject(NativePty.open(options.toString()))
            check(opened.optBoolean("ok")) { opened.optString("error", "本机初始化失败") }
            handle = opened.getJSONObject("data").getLong("handle")
            createWebView()
        } catch (_: Throwable) {
            if (handle != 0L) { NativePty.close(handle); handle = 0L }
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
                handler.post { if (foreground && ready && view.url == page) action(json) }
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
                ready = url == page
                if (ready && foreground) { handler.removeCallbacks(poll); handler.post(poll) }
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
        try {
            val action = JSONObject(json)
            if (action.optString("op") in listOf("enroll", "connect")) {
                val password = action.optString("password").ifEmpty { PasswordStore.load(this).orEmpty() }
                action.put("password", password)
                if (action.optString("op") == "enroll") pendingPassword = password
            }
            val result = JSONObject(NativePty.send(handle, action.toString()))
            if (!result.optBoolean("ok")) { pendingPassword = null; dispatch(JSONObject().put("type", "error").put("message", result.optString("error"))) }
        } catch (_: Exception) { pendingPassword = null; dispatch(JSONObject().put("type", "error").put("message", "本机操作失败")) }
    }
    private val poll = object : Runnable {
        override fun run() {
            if (!foreground || !ready || handle == 0L) return
            if (rendering) { handler.postDelayed(this, 25); return }
            try {
                val result = JSONObject(NativePty.poll(handle))
                if (result.optBoolean("ok")) {
                    val events = result.getJSONArray("data")
                    for (index in 0 until events.length()) {
                        val event = events.getJSONObject(index)
                        if (event.optString("type") == "progress" && event.optJSONObject("progress")?.optString("phase") == "installed") {
                            pendingPassword?.let { PasswordStore.save(this@MainActivity, it) }; pendingPassword = null
                        }
                        if (event.optString("type") == "error") pendingPassword = null
                    }
                    if (events.length() > 0) {
                        rendering = true
                        web?.evaluateJavascript("window.flowsplice.receiveBatch(JSON.parse(${JSONObject.quote(events.toString())})).finally(() => window.FlowSpliceNative.rendered());", null)
                    }
                } else dispatch(JSONObject().put("type", "error").put("message", result.optString("error")))
            } catch (_: Exception) { dispatch(JSONObject().put("type", "error").put("message", "本机事件读取失败")) }
            handler.postDelayed(this, 25)
        }
    }
    override fun onResume() { super.onResume(); foreground = true; if (ready) handler.post(poll) }
    override fun onStop() {
        foreground = false
        if (handle != 0L) NativePty.send(handle, "{\"op\":\"disconnect\"}")
        handler.removeCallbacks(poll)
        pendingPassword = null
        super.onStop()
    }
    override fun onDestroy() {
        foreground = false; ready = false; handler.removeCallbacks(poll)
        if (handle != 0L) { NativePty.close(handle); handle = 0L }
        web?.removeJavascriptInterface("FlowSpliceNative"); web?.destroy(); web = null
        pendingPassword = null
        super.onDestroy()
    }
}

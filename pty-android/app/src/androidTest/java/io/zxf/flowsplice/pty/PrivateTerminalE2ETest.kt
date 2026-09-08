package io.zxf.flowsplice.pty

import android.graphics.Bitmap
import android.graphics.Canvas
import android.view.KeyEvent
import android.view.ViewGroup
import android.view.inputmethod.EditorInfo
import android.webkit.WebView
import androidx.lifecycle.Lifecycle
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

/** Only the external orchestrator approves enrollment; no transport or Rust test backdoor. */
@RunWith(AndroidJUnit4::class)
class PrivateTerminalE2ETest {
    private fun evaluate(scenario: ActivityScenario<MainActivity>, expression: String): String {
        val done = CountDownLatch(1)
        val result = AtomicReference<String>()
        scenario.onActivity { activity ->
            val content = activity.findViewById<ViewGroup>(android.R.id.content)
            val web = content.getChildAt(0) as? WebView
            assertNotNull("Private configuration did not produce a WebView", web)
            web!!.evaluateJavascript(expression) { value -> result.set(value); done.countDown() }
        }
        assertTrue("WebView evaluation timed out", done.await(5, TimeUnit.SECONDS))
        return result.get()
    }

    private fun waitFor(scenario: ActivityScenario<MainActivity>, expression: String, checkpoint: String, seconds: Long = 30) {
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(seconds)
        while (System.nanoTime() < deadline) {
            if (evaluate(scenario, expression) == "true") return
            Thread.sleep(100)
        }
        val diagnostic = evaluate(scenario, """
            (()=>({
                status:document.getElementById('status')?.textContent,
                notice:document.getElementById('notice')?.textContent,
                terminalAccessibility:Array.from(document.querySelectorAll('#panes > .terminal:not([hidden]) .xterm-rows,#panes > .terminal:not([hidden]) .xterm-accessibility')).map(e=>e.textContent),
                terminalTextarea:document.querySelector('#panes > .terminal:not([hidden]) .xterm-helper-textarea')?.value,
                activeElementClass:document.activeElement?.className
            }))()
        """.trimIndent())
        scenario.onActivity { activity ->
            activity.openFileOutput("e2e-diagnostic.json", android.content.Context.MODE_PRIVATE).use {
                it.write(diagnostic.toByteArray(Charsets.UTF_8))
            }
        }
        screenshot(scenario)
        fail("Timed out at $checkpoint")
    }

    private fun screenshot(scenario: ActivityScenario<MainActivity>) {
    scenario.onActivity { activity ->
        val web = activity.findViewById<ViewGroup>(android.R.id.content).getChildAt(0) as WebView
        assertTrue("Terminal WebView must be visible for evidence", web.isShown && web.width > 0 && web.height > 0)
        val bitmap = Bitmap.createBitmap(web.width, web.height, Bitmap.Config.ARGB_8888)
        try {
            web.draw(Canvas(bitmap))
            activity.openFileOutput("e2e-terminal.png", android.content.Context.MODE_PRIVATE).use { stream ->
                assertTrue("Terminal screenshot encoding failed", bitmap.compress(Bitmap.CompressFormat.PNG, 100, stream))
            }
        } finally {
            bitmap.recycle()
        }
    }
    }

    private fun click(scenario: ActivityScenario<MainActivity>, id: String) {
        val quoted = JSONObject.quote(id)
        assertEquals("Required UI control unavailable: $id", "true", evaluate(scenario,
            "(()=>{const e=document.getElementById($quoted);if(!e||e.hidden||e.disabled)return false;e.click();return true})()"))
    }

    private fun typeTerminal(scenario: ActivityScenario<MainActivity>, command: String) {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        scenario.onActivity { activity ->
            val web = activity.findViewById<ViewGroup>(android.R.id.content).getChildAt(0) as WebView
            assertTrue("WebView native focus unavailable", web.requestFocus())
        }
        assertEquals("Terminal textarea is unavailable", "true", evaluate(scenario,
            "(()=>{const e=document.querySelector('#panes > .terminal:not([hidden]) .xterm-helper-textarea');if(!e)return false;e.focus();return document.activeElement===e})()"))
        instrumentation.waitForIdleSync()
        Thread.sleep(250)
        scenario.onActivity { activity ->
            val web = activity.findViewById<ViewGroup>(android.R.id.content).getChildAt(0) as WebView
            val connection = web.onCreateInputConnection(EditorInfo())
            assertNotNull("WebView terminal does not expose an IME input connection", connection)
            assertTrue("Terminal IME rejected text", connection!!.commitText(command, 1))
            assertTrue("Terminal IME rejected composition completion", connection.finishComposingText())
        }
        instrumentation.waitForIdleSync()
        Thread.sleep(250)
        scenario.onActivity { activity ->
            val web = activity.findViewById<ViewGroup>(android.R.id.content).getChildAt(0) as WebView
            val connection = web.onCreateInputConnection(EditorInfo())
            assertNotNull("Terminal IME connection unavailable for Return", connection)
            assertTrue("Terminal IME rejected Return", connection!!.sendKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_ENTER)))
            assertTrue(connection.sendKeyEvent(KeyEvent(KeyEvent.ACTION_UP, KeyEvent.KEYCODE_ENTER)))
        }
    }

    @Test fun enrollCreateAndResumeExistingSession() {
        val args = InstrumentationRegistry.getArguments()
        assumeTrue("Requires an explicitly isolated device/app installation", args.getString("isolated") == "true")
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        assumeTrue("Requires externally packaged private bootstrap", context.assets.list("bootstrap").orEmpty().contains("homes.json"))
        val relay = args.getString("ptyRelay") ?: error("External ptyRelay argument required")
        val passwordPath = args.getString("ptyPasswordFile") ?: error("External ptyPasswordFile argument required")
        assertTrue("Password fixture path must be absolute", File(passwordPath).isAbsolute)
        val password = File(passwordPath).readText().trimEnd('\r', '\n')
        assertTrue("Password fixture must be nonempty", password.isNotEmpty())
        assertFalse("Requires a fresh isolated installation", context.filesDir.resolve("installation/travelagent.toml").exists())
        ActivityScenario.launch(MainActivity::class.java).use { scenario ->
            fun management() { click(scenario, "terminal-back") }
            fun select(id: String) {
                if (evaluate(scenario, "!document.getElementById('terminal-view').hidden") == "true") management()
                click(scenario, "mobile-home")
                assertEquals("true", evaluate(scenario, "(()=>{const b=document.querySelector('#home-cards [data-home-id=\"$id\"]');if(!b)return false;b.click();return true})()"))
            }
            fun enroll(id: String) {
                select(id)
                waitFor(scenario, "document.getElementById('enter').textContent==='注册'", "enrollment form $id")
                assertEquals("true", evaluate(scenario,
                    "(()=>{document.getElementById('relay').value=${JSONObject.quote(relay)};document.getElementById('password').value=${JSONObject.quote(password)};return true})()"))
                click(scenario, "enter")
                waitFor(scenario, "!document.getElementById('waiting').hidden&&!document.getElementById('verification-code').textContent.includes('正在')", "approval code $id", 120)
                context.filesDir.resolve("e2e-verification.json").writeText(evaluate(scenario, "'等待批准，校验码：'+document.getElementById('verification-code').textContent"))
                waitFor(scenario, "document.getElementById('status').textContent.includes('已连接')", "automatic post-enrollment connection $id", 180)
                assertEquals("true", evaluate(scenario, "document.getElementById('password-label').hidden"))
                waitFor(scenario, "document.querySelectorAll('#list .session').length===0", "empty Home $id")
            }
            fun create(): String {
                click(scenario, "new")
                click(scenario, "cancel-new")
                assertEquals("0", evaluate(scenario, "document.querySelectorAll('#list .session').length"))
                click(scenario, "new")
                assertEquals("true", evaluate(scenario, "(()=>{document.getElementById('session-name').value='E2E shell';return true})()"))
                click(scenario, "confirm-new")
                waitFor(scenario, "!document.getElementById('terminal-view').hidden&&document.getElementById('mode-label').textContent==='读写'", "named writable terminal")
                management()
                waitFor(scenario, "document.querySelector('#list .session-name')?.textContent==='E2E shell'&&document.querySelector('#list .connection')?.textContent.includes('1 个连接')", "name and attachment metadata")
                assertEquals("true", evaluate(scenario, "Array.from(document.querySelectorAll('#list .session-time')).length===2&&Array.from(document.querySelectorAll('#list .session-time')).every(e=>e.textContent&&!e.textContent.includes('尚未'))"))
                val session = evaluate(scenario, "document.querySelector('#list .session').dataset.sessionId")
                assertEquals("true", evaluate(scenario, "(()=>{const b=Array.from(document.querySelectorAll('#list button')).find(e=>e.textContent==='继续');if(!b)return false;b.click();return true})()"))
                return session
            }
            fun output(marker: String) {
                val octal = marker.toByteArray(Charsets.US_ASCII).joinToString("") { "\\%03o".format(it.toInt() and 255) }
                typeTerminal(scenario, "printf '\\n${octal}\\n'")
                waitFor(scenario, "Array.from(document.querySelectorAll('#panes > .terminal:not([hidden]) .xterm-rows,#panes > .terminal:not([hidden]) .xterm-accessibility')).some(e=>e.textContent.includes(${JSONObject.quote(marker)}))", "remote rendered output $marker", 30)
            }
            fun switch(name: String) {
                click(scenario, if (evaluate(scenario, "!document.getElementById('terminal-view').hidden") == "true") "terminal-switcher" else "mobile-terminals")
                assertEquals("true", evaluate(scenario, "(()=>{const b=Array.from(document.querySelectorAll('#opened-list button')).find(e=>e.textContent===${JSONObject.quote(name + " / E2E shell")});if(!b)return false;b.click();return true})()"))
            }
            waitFor(scenario, "document.querySelectorAll('#home-cards [data-home-id]').length===2", "two Home catalog")
            enroll("default")
            val first = create()
            assertEquals("7", evaluate(scenario, "document.querySelectorAll('#keys button').length"))
            val marker = "PTY_E2E_" + UUID.randomUUID().toString().replace("-", "").take(8)
            output(marker)
            click(scenario, "mode")
            waitFor(scenario, "document.getElementById('mode-label').textContent==='只读'", "readonly")
            click(scenario, "mode")
            waitFor(scenario, "document.getElementById('mode-label').textContent==='读写'", "writer restored")
            enroll("secondary")
            val second = create()
            assertNotEquals("Same names on different Homes must have different session identities", first, second)
            output(marker + "SECOND")
            assertEquals("2", evaluate(scenario, "document.querySelectorAll('#tabs button').length"))
            switch("测试 Mac"); output(marker + "FIRST")
            switch("测试 VPS"); output(marker + "SECONDAGAIN")
            management(); click(scenario, "disconnect")
            waitFor(scenario, "document.querySelectorAll('#tabs button').length===1", "one Home disconnect preserves other tab")
            switch("测试 Mac"); output(marker + "SURVIVED")
            // Reconnect only the secondary Home, list its shell, and explicitly rejoin.
            select("secondary")
            waitFor(scenario, "document.getElementById('status').textContent.includes('已连接')&&document.querySelectorAll('#list .session').length===1", "secondary reconnect", 180)
            assertEquals(second, evaluate(scenario, "document.querySelector('#list .session').dataset.sessionId"))
            assertEquals("true", evaluate(scenario, "(()=>{const b=Array.from(document.querySelectorAll('#list button')).find(e=>e.textContent==='打开');if(!b)return false;b.click();return true})()"))
            waitFor(scenario, "document.querySelectorAll('#tabs button').length===2", "two joined Homes before background")
            scenario.moveToState(Lifecycle.State.CREATED)
            scenario.moveToState(Lifecycle.State.RESUMED)
            waitFor(scenario, "document.querySelectorAll('#tabs button').length===0", "background disconnects all Homes")
            repeat(20) {
                assertEquals("No automatic foreground reconnect", "true", evaluate(scenario, "!document.getElementById('status').textContent.includes('已连接')"))
                Thread.sleep(100)
            }
            click(scenario, "enter")
            waitFor(scenario, "document.getElementById('status').textContent.includes('已连接')&&document.querySelectorAll('#list .session').length===1", "foreground explicit reconnect", 180)
            assertEquals(second, evaluate(scenario, "document.querySelector('#list .session').dataset.sessionId"))
            assertEquals("0", evaluate(scenario, "document.querySelectorAll('#tabs button').length"))
            assertEquals("true", evaluate(scenario, "(()=>{const b=Array.from(document.querySelectorAll('#list button')).find(e=>e.textContent==='打开');if(!b)return false;b.click();return true})()"))
            waitFor(scenario, "Array.from(document.querySelectorAll('#panes > .terminal:not([hidden]) .xterm-rows')).some(e=>e.textContent.includes(${JSONObject.quote(marker + "SECONDAGAIN")}))", "tmux restores prior output")
            output(marker + "RESUMED")
            screenshot(scenario)
            management(); click(scenario, "disconnect")
        }
    }
}

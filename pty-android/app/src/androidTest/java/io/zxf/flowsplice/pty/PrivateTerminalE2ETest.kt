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
                terminalAccessibility:Array.from(document.querySelectorAll('.xterm-rows,.xterm-accessibility')).map(e=>e.textContent),
                terminalTextarea:document.querySelector('.xterm-helper-textarea')?.value,
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
            "(()=>{const e=document.querySelector('.xterm-helper-textarea');if(!e)return false;e.focus();return document.activeElement===e})()"))
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
        assumeTrue("Requires externally packaged private bootstrap", context.assets.list("bootstrap").orEmpty().contains("business.json"))
        val relay = args.getString("ptyRelay") ?: error("External ptyRelay argument required")
        val passwordPath = args.getString("ptyPasswordFile") ?: error("External ptyPasswordFile argument required")
        assertTrue("Password fixture path must be absolute", File(passwordPath).isAbsolute)
        val password = File(passwordPath).readText().trimEnd('\r', '\n')
        assertTrue("Password fixture must be nonempty", password.isNotEmpty())
        assertFalse("Requires a fresh isolated installation", context.filesDir.resolve("installation/travelagent.toml").exists())
        ActivityScenario.launch(MainActivity::class.java).use { scenario ->
            waitFor(scenario, "document.getElementById('enter')?.textContent==='注册'", "initial enrollment form")
            // Drive the same DOM form and native message bridge used by the actual user UI.
            assertEquals("true", evaluate(scenario,
                "(()=>{document.getElementById('relay').value=${JSONObject.quote(relay)};document.getElementById('password').value=${JSONObject.quote(password)};return true})()"))
            click(scenario, "enter")
            waitFor(scenario, "document.getElementById('notice').textContent.includes('等待批准')", "external approval pending", 60)
            context.filesDir.resolve("e2e-verification.json").writeText(evaluate(scenario, "document.getElementById('notice').textContent"))
            waitFor(scenario, "document.getElementById('enter').textContent==='连接'&&!document.getElementById('enter').disabled", "externally approved installation", 120)
            click(scenario, "enter")
            waitFor(scenario, "document.getElementById('status').textContent==='已连接'", "connected")
            waitFor(scenario, "document.getElementById('list').textContent==='暂无会话'", "empty fixture session list")
            click(scenario, "new")
            waitFor(scenario, "!document.getElementById('terminal-controls').hidden&&document.getElementById('mode-label').textContent==='读写'", "new writable terminal")
            val session = evaluate(scenario, "document.querySelector('#tabs button')?.textContent")
            assertNotEquals("null", session)
            waitFor(scenario,
                "document.querySelectorAll('#list .session').length===1&&document.querySelector('#list .session span')?.textContent===$session",
                "new session immediately appears in refreshed list")
            val marker = "PTY_E2E_" + UUID.randomUUID().toString().replace("-", "").take(8)
            val octal = marker.toByteArray(Charsets.US_ASCII).joinToString("") { "\\%03o".format(it.toInt() and 255) }
            typeTerminal(scenario, "printf '\\n${octal}\\n'")
            // The source command contains only octal bytes; finding decoded text proves output.
            waitFor(scenario,
                "Array.from(document.querySelectorAll('.xterm-rows,.xterm-accessibility')).some(e=>e.textContent.includes(${JSONObject.quote(marker)}))",
                "decoded rendered terminal output (DOM accessibility must be available)", 20)
            screenshot(scenario)
            click(scenario, "mode")
            waitFor(scenario, "document.getElementById('mode-label').textContent==='只读'", "read-only ownership")
            click(scenario, "mode")
            waitFor(scenario, "document.getElementById('mode-label').textContent==='读写'", "restored writer")
            scenario.moveToState(Lifecycle.State.CREATED)
            scenario.moveToState(Lifecycle.State.RESUMED)
            waitFor(scenario, "document.getElementById('status').textContent==='未连接'", "foreground disconnected")
            repeat(20) {
                assertEquals("No automatic reconnect", "true", evaluate(scenario, "document.getElementById('status').textContent==='未连接'"))
                Thread.sleep(100)
            }
            click(scenario, "enter")
            waitFor(scenario, "document.getElementById('status').textContent==='已连接'&&document.querySelectorAll('#list .session').length===1", "existing session list")
            assertEquals("No unsolicited terminal join", "0", evaluate(scenario, "document.querySelectorAll('#tabs button').length"))
            assertEquals("Reconnect must retain the same session", session, evaluate(scenario, "document.querySelector('#list .session span').textContent"))
            assertEquals("true", evaluate(scenario, "(()=>{const b=Array.from(document.querySelectorAll('#list button')).find(e=>e.textContent==='读写加入');if(!b)return false;b.click();return true})()"))
            waitFor(scenario, "document.getElementById('mode-label').textContent==='读写'&&!document.getElementById('terminal-controls').hidden", "explicit rejoin")
            assertEquals(session, evaluate(scenario, "document.querySelector('#tabs button').textContent"))
            click(scenario, "disconnect")
            waitFor(scenario, "document.getElementById('status').textContent==='未连接'", "final disconnect")
        }
    }
}

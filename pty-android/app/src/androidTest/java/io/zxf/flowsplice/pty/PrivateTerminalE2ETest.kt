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
import org.json.JSONArray
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
                identityStatus:document.getElementById('identity-status')?.textContent,
                globalNotice:document.getElementById('global-notice')?.textContent,
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

    private fun imeDiagnostic(scenario: ActivityScenario<MainActivity>, stage: String, phase: String, connectionAvailable: Boolean? = null) {
        // Deliberately omit values, terminal text, commands and credentials.
        val dom = evaluate(scenario, """
            (()=>({
                activeElementClass:document.activeElement?.className,
                activeElementTag:document.activeElement?.tagName,
                documentHasFocus:document.hasFocus(),
                identityStatus:document.getElementById('identity-status')?.textContent,
                globalNotice:document.getElementById('global-notice')?.textContent,
                status:document.getElementById('status')?.textContent,
                notice:document.getElementById('notice')?.textContent,
                terminalNotice:document.getElementById('terminal-notice')?.textContent,
                tabCount:document.querySelectorAll('#tabs button').length,
                page:document.getElementById('app')?.dataset.page,
                terminalViewVisible:!document.getElementById('terminal-view')?.hidden,
                visibleTerminalCount:document.querySelectorAll('#panes > .terminal:not([hidden])').length,
                terminalTextareaPresent:!!document.querySelector('#panes > .terminal:not([hidden]) .xterm-helper-textarea'),
                terminalTextareaFocused:document.activeElement===document.querySelector('#panes > .terminal:not([hidden]) .xterm-helper-textarea')
            }))()
        """.trimIndent())
        scenario.onActivity { activity ->
            val web = activity.findViewById<ViewGroup>(android.R.id.content).getChildAt(0) as WebView
            val record = JSONObject().put("stage", stage).put("phase", phase)
                .put("elapsedRealtimeMs", android.os.SystemClock.elapsedRealtime())
                .put("dom", JSONObject(dom)).put("webHasFocus", web.hasFocus())
                .put("webHasWindowFocus", web.hasWindowFocus()).put("webIsShown", web.isShown)
                .put("webIsTextEditor", web.onCheckIsTextEditor())
                .put("connectionAvailable", connectionAvailable ?: JSONObject.NULL)
            activity.openFileOutput("e2e-ime-phases.jsonl", android.content.Context.MODE_APPEND).use {
                it.write((record.toString() + "\n").toByteArray(Charsets.UTF_8))
            }
        }
    }

    private fun typeTerminal(scenario: ActivityScenario<MainActivity>, command: String, stage: String) {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        imeDiagnostic(scenario, stage, "before-native-focus")
        try {
            scenario.onActivity { activity ->
                val web = activity.findViewById<ViewGroup>(android.R.id.content).getChildAt(0) as WebView
                assertTrue("WebView native focus unavailable [$stage]", web.requestFocus())
            }
        } finally { imeDiagnostic(scenario, stage, "after-native-focus") }
        imeDiagnostic(scenario, stage, "before-dom-focus")
        try {
            assertEquals("Terminal textarea is unavailable [$stage]", "true", evaluate(scenario,
                "(()=>{const e=document.querySelector('#panes > .terminal:not([hidden]) .xterm-helper-textarea');if(!e)return false;e.focus();return document.activeElement===e})()"))
        } finally { imeDiagnostic(scenario, stage, "after-dom-focus") }
        instrumentation.waitForIdleSync()
        Thread.sleep(250)
        var connectionAvailable: Boolean? = null
        imeDiagnostic(scenario, stage, "before-commit")
        try {
            scenario.onActivity { activity ->
                val web = activity.findViewById<ViewGroup>(android.R.id.content).getChildAt(0) as WebView
                val connection = web.onCreateInputConnection(EditorInfo())
                connectionAvailable = connection != null
                assertNotNull("WebView terminal does not expose an IME input connection [$stage]", connection)
                assertTrue("Terminal IME rejected text [$stage]", connection!!.commitText(command, 1))
                assertTrue("Terminal IME rejected composition completion [$stage]", connection.finishComposingText())
            }
        } finally { imeDiagnostic(scenario, stage, "after-commit-and-composition", connectionAvailable) }
        instrumentation.waitForIdleSync()
        Thread.sleep(250)
        connectionAvailable = null
        imeDiagnostic(scenario, stage, "before-return")
        try {
            scenario.onActivity { activity ->
                val web = activity.findViewById<ViewGroup>(android.R.id.content).getChildAt(0) as WebView
                val connection = web.onCreateInputConnection(EditorInfo())
                connectionAvailable = connection != null
                assertNotNull("Terminal IME connection unavailable for Return [$stage]", connection)
                assertTrue("Terminal IME rejected Return [$stage]", connection!!.sendKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_ENTER)))
                assertTrue(connection.sendKeyEvent(KeyEvent(KeyEvent.ACTION_UP, KeyEvent.KEYCODE_ENTER)))
            }
        } finally { imeDiagnostic(scenario, stage, "after-return", connectionAvailable) }
    }

    @Test fun enrollCreateAndResumeExistingSession() {
        val args = InstrumentationRegistry.getArguments()
        assumeTrue("Requires an explicitly isolated device/app installation", args.getString("isolated") == "true")
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val isClass = args.getString("ptyServiceClass") == "true"
        assumeTrue("Requires externally packaged private bootstrap", context.assets.list("bootstrap").orEmpty().contains(if (isClass) "service-class.json" else "homes.json"))
        fun targetArg(name: String): String = String(android.util.Base64.decode(args.getString(name + "Base64") ?: error("Missing target argument $name"), android.util.Base64.NO_WRAP), Charsets.UTF_8)
        val firstId = if (isClass) targetArg("ptyFirstHomeId") else "default"
        val secondId = if (isClass) targetArg("ptySecondHomeId") else "secondary"
        val firstName = if (isClass) targetArg("ptyFirstHomeName") else "测试 Mac"
        val secondName = if (isClass) targetArg("ptySecondHomeName") else "测试 VPS"
        val phase = args.getString("ptyPhase") ?: error("Explicit ptyPhase prepare/restore required")
        require(phase in listOf("prepare", "restore"))
        val metadataFile = context.filesDir.resolve("e2e-process-restart.json")
        val workspaceFile = context.filesDir.resolve(if (isClass) "pty-workspace-class-v1.json" else "pty-workspace-legacy-v1.json")
        val relay = args.getString("ptyRelay").orEmpty()
        val password = if (phase == "prepare") {
            val passwordPath = args.getString("ptyPasswordFile") ?: error("External ptyPasswordFile argument required")
            assertTrue("Password fixture path must be absolute", File(passwordPath).isAbsolute)
            assertFalse("Requires a fresh isolated installation", context.filesDir.resolve(if (isClass) "service-class/travelagent.toml" else "installation/travelagent.toml").exists())
            File(passwordPath).readText().trimEnd('\r', '\n').also { assertTrue("Password fixture must be nonempty", it.isNotEmpty()) }
        } else {
            assertTrue("Restart metadata required from successful prepare phase", metadataFile.isFile)
            assertFalse("Restore must not have access to the fixture password", context.filesDir.resolve("fixture-password").exists())
            ""
        }
        ActivityScenario.launch(MainActivity::class.java).use { scenario ->
            fun management() { click(scenario, "terminal-back") }
            fun select(id: String) {
                if (evaluate(scenario, "!document.getElementById('terminal-view').hidden") == "true") management()
                click(scenario, "mobile-home")
                waitFor(scenario, "Array.from(document.querySelectorAll('#home-cards [data-home-id]')).some(e=>e.dataset.homeId===${JSONObject.quote(id)})", "target discovered $id")
                assertEquals("true", evaluate(scenario, "(()=>{const b=Array.from(document.querySelectorAll('#home-cards [data-home-id]')).find(e=>e.dataset.homeId===${JSONObject.quote(id)});if(!b)return false;b.click();return true})()"))
            }
            fun enrollClass() {
                waitFor(scenario, "!document.getElementById('identity-panel').hidden", "class identity setup")
                assertEquals("true", evaluate(scenario, "(()=>{document.getElementById('identity-relay').value=${JSONObject.quote(relay)};document.getElementById('identity-password').value=${JSONObject.quote(password)};return true})()"))
                click(scenario, "identity-enter")
                waitFor(scenario, "!document.getElementById('identity-waiting').hidden&&!document.getElementById('identity-code').textContent.includes('正在')", "class approval code", 120)
                context.filesDir.resolve("e2e-verification.json").writeText(evaluate(scenario, "({notice:'等待批准，校验码：'+document.getElementById('identity-code').textContent,client_label:document.getElementById('identity-label').textContent})"))
                waitFor(scenario, "document.getElementById('identity-status').textContent==='服务目录已连接'", "shared class connection", 180)
                assertEquals("0", evaluate(scenario, "document.querySelectorAll('#tabs button').length"))
            }
            fun connectHome(id: String) {
                select(id)
                waitFor(scenario, "document.getElementById('status').textContent.includes('已连接')&&!document.getElementById('new').disabled", "class target connection", 180)
                assertEquals("true", evaluate(scenario, "document.getElementById('password-label').hidden&&document.getElementById('relay-label').hidden"))
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
                click(scenario, "rename-terminal")
                waitFor(scenario, "document.getElementById('rename-name').value==='E2E shell'", "prefilled rename")
                evaluate(scenario, "document.getElementById('rename-name').value='Cancelled name'")
                click(scenario, "cancel-rename")
                management()
                assertEquals("true", evaluate(scenario, "document.querySelector('#list .session-name').textContent==='E2E shell'"))
                assertEquals("true", evaluate(scenario, "(()=>{const b=Array.from(document.querySelectorAll('#list button')).find(e=>e.textContent==='重命名');if(!b)return false;b.click();return true})()"))
                waitFor(scenario, "document.getElementById('rename-name').value==='E2E shell'", "list rename prefilled")
                evaluate(scenario, "document.getElementById('rename-name').value='E2E renamed'")
                click(scenario, "save-rename")
                waitFor(scenario, "document.querySelector('#list .session-name')?.textContent==='E2E renamed'", "renamed session list")
                assertEquals(session, evaluate(scenario, "document.querySelector('#list .session').dataset.sessionId"))
                assertEquals("true", evaluate(scenario, "(()=>{const b=Array.from(document.querySelectorAll('#list button')).find(e=>e.textContent==='继续');if(!b)return false;b.click();return true})()"))
                waitFor(scenario, "Array.from(document.querySelectorAll('#tabs button')).some(e=>e.textContent.includes('E2E renamed'))&&document.getElementById('mode-label').textContent==='读写'", "renamed writable terminal tab")
                return session
            }
            fun output(marker: String, stage: String) {
                val octal = marker.toByteArray(Charsets.US_ASCII).joinToString("") { "\\%03o".format(it.toInt() and 255) }
                typeTerminal(scenario, "printf '\\n${octal}\\n'", stage)
                waitFor(scenario, "Array.from(document.querySelectorAll('#panes > .terminal:not([hidden]) .xterm-rows,#panes > .terminal:not([hidden]) .xterm-accessibility')).some(e=>e.textContent.includes(${JSONObject.quote(marker)}))", "remote rendered output $marker", 30)
            }
            fun switch(name: String) {
                click(scenario, if (evaluate(scenario, "!document.getElementById('terminal-view').hidden") == "true") "terminal-switcher" else "mobile-terminals")
                assertEquals("true", evaluate(scenario, "(()=>{const b=Array.from(document.querySelectorAll('#opened-list button')).find(e=>e.textContent===${JSONObject.quote(name + " / E2E renamed")});if(!b)return false;b.click();return true})()"))
            }
            if (phase == "prepare") {
            if (isClass) {
                enrollClass()
                waitFor(scenario, "[${JSONObject.quote(firstId)},${JSONObject.quote(secondId)}].every(id=>Array.from(document.querySelectorAll('#home-cards [data-home-id]')).some(e=>e.dataset.homeId===id))", "two discovered Home targets")
                connectHome(firstId)
            } else {
                waitFor(scenario, "document.querySelectorAll('#home-cards [data-home-id]').length===2", "two Home catalog")
                enroll(firstId)
            }
            val first = create()
            assertEquals("7", evaluate(scenario, "document.querySelectorAll('#keys button').length"))
            val marker = "PTY_E2E_" + UUID.randomUUID().toString().replace("-", "").take(8)
            output(marker, "first-home-initial")
            click(scenario, "mode")
            waitFor(scenario, "document.getElementById('mode-label').textContent==='只读'", "readonly")
            click(scenario, "mode")
            waitFor(scenario, "document.getElementById('mode-label').textContent==='读写'", "writer restored")
            if (isClass) connectHome(secondId) else enroll(secondId)
            val second = create()
            assertNotEquals("Same names on different Homes must have different session identities", first, second)
            output(marker + "SECOND", "second-home-initial")
            assertEquals("2", evaluate(scenario, "document.querySelectorAll('#tabs button').length"))
            switch(firstName); output(marker + "FIRST", "first-home-switched")
            switch(secondName); output(marker + "SECONDAGAIN", "second-home-switched")
            management(); click(scenario, "disconnect")
            waitFor(scenario, "document.querySelectorAll('#tabs button').length===1", "one Home disconnect preserves other tab")
            switch(firstName); output(marker + "SURVIVED", "first-home-survived-disconnect")
            // Reconnect only the secondary Home, list its shell, and explicitly rejoin.
            select(secondId)
            waitFor(scenario, "document.getElementById('status').textContent.includes('已连接')&&document.querySelectorAll('#list .session').length===1", "secondary reconnect", 180)
            assertEquals(second, evaluate(scenario, "document.querySelector('#list .session').dataset.sessionId"))
            assertEquals("true", evaluate(scenario, "(()=>{const b=Array.from(document.querySelectorAll('#list button')).find(e=>e.textContent==='打开');if(!b)return false;b.click();return true})()"))
            waitFor(scenario, "document.querySelectorAll('#tabs button').length===2", "two joined Homes before background")
            scenario.moveToState(Lifecycle.State.CREATED)
            Thread.sleep(1500)
            scenario.moveToState(Lifecycle.State.RESUMED)
            waitFor(scenario, "document.querySelectorAll('#tabs button').length===2", "short background retains both tabs")
            output(marker + "RESUMED", "second-home-foreground-resumed")
            scenario.moveToState(Lifecycle.State.CREATED)
            Thread.sleep(35_000)
            scenario.moveToState(Lifecycle.State.RESUMED)
            waitFor(scenario, "document.querySelectorAll('#tabs button').length===2&&document.getElementById('mode-label').textContent==='读写'", "automatic restoration beyond background grace", 180)
            output(marker + "LONG", "second-home-long-background")
            click(scenario, "mode")
            waitFor(scenario, "document.getElementById('mode-label').textContent==='只读'", "readonly before recreation")
            scenario.recreate()
            waitFor(scenario, "document.querySelectorAll('#tabs button').length===2&&document.getElementById('mode-label').textContent==='只读'", "workspace and readonly restored", 180)
            assertEquals("true", evaluate(scenario, "Array.from(document.querySelectorAll('#tabs button')).some(e=>e.dataset.sessionId===" + second + ")"))
            val firstSession = JSONArray("[$first]").getString(0)
            val secondSession = JSONArray("[$second]").getString(0)
            UUID.fromString(firstSession); UUID.fromString(secondSession)
            val savedDeadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10)
            var saved = false
            while (System.nanoTime() < savedDeadline && !saved) {
                saved = runCatching {
                    val value = JSONObject(workspaceFile.readText())
                    val tabs = value.getJSONArray("tabs")
                    tabs.length() == 2 && (0 until tabs.length()).all { index ->
                        val tab = tabs.getJSONObject(index)
                        (tab.getString("home") == firstId && tab.getString("session") == firstSession && tab.getString("mode") == "read_write") ||
                            (tab.getString("home") == secondId && tab.getString("session") == secondSession && tab.getString("mode") == "read_only")
                    } && value.getJSONObject("active").getString("session") == secondSession && value.getString("selected") == secondId
                }.getOrDefault(false)
                if (!saved) Thread.sleep(100)
            }
            assertTrue("Expected two-tab RO workspace was not durably saved", saved)
            metadataFile.writeText(JSONObject().put("version", 1).put("first", firstSession).put("second", secondSession)
                .put("firstHome", firstId).put("secondHome", secondId).put("marker", marker).toString())
            return@use
            }
            val metadata = JSONObject(metadataFile.readText())
            assertEquals(1, metadata.getInt("version"))
            assertEquals(firstId, metadata.getString("firstHome")); assertEquals(secondId, metadata.getString("secondHome"))
            val firstSession = UUID.fromString(metadata.getString("first")).toString()
            val secondSession = UUID.fromString(metadata.getString("second")).toString()
            val marker = metadata.getString("marker")
            waitFor(scenario, "document.querySelectorAll('#tabs button').length===2&&Array.from(document.querySelectorAll('#tabs button')).every(e=>e.dataset.state==='attached')&&document.getElementById('mode-label').textContent==='只读'&&document.querySelector('#tabs button[aria-selected=true]')?.dataset.sessionId===${JSONObject.quote(secondSession)}&&document.querySelector('#tabs button[aria-selected=true]')?.dataset.homeId===${JSONObject.quote(secondId)}", "automatic process restart restores selected readonly tab", 180)
            assertEquals("true", evaluate(scenario, "Array.from(document.querySelectorAll('#tabs button')).some(e=>e.dataset.sessionId===${JSONObject.quote(firstSession)}&&e.dataset.homeId===${JSONObject.quote(firstId)})"))
            switch(firstName)
            waitFor(scenario, "['只读','读写'].includes(document.getElementById('mode-label').textContent)", "first Home attached after process restore")
            if (evaluate(scenario, "document.getElementById('mode-label').textContent==='只读'") == "true") {
                // A surviving writer lease may legitimately degrade automatic RW restore to RO.
                // Request and, only when prompted, explicitly confirm write takeover through UI.
                click(scenario, "mode")
                waitFor(scenario, "document.getElementById('mode-label').textContent==='读写'||document.getElementById('takeover').open", "first Home explicit writer request")
                if (evaluate(scenario, "document.getElementById('takeover').open") == "true") click(scenario, "confirm-takeover")
                waitFor(scenario, "document.getElementById('mode-label').textContent==='读写'", "first Home explicit writer reacquired")
            }
            output(marker + "PROCESSFIRST", "first-home-process-restored")
            switch(secondName)
            waitFor(scenario, "document.getElementById('mode-label').textContent==='只读'", "second Home readonly mode retained")
            click(scenario, "mode")
            waitFor(scenario, "document.getElementById('mode-label').textContent==='读写'", "writer after process restart")
            output(marker + "PROCESS", "second-home-process-restored")
            scenario.onActivity { activity -> activity.openFileOutput("e2e-delete-second", android.content.Context.MODE_PRIVATE).use { it.write("delete".toByteArray()) } }
            waitFor(scenario, "document.getElementById('mode-label').textContent==='已删除'&&document.getElementById('mode').disabled&&document.querySelectorAll('#tabs button').length===2", "externally deleted session keeps tombstone tab")
            switch(firstName); output(marker + "AFTERDELETE", "other-home-survives-deletion")
            screenshot(scenario)
            management(); click(scenario, "disconnect")
            context.filesDir.resolve("e2e-process-restored.json").writeText(JSONObject().put("version", 1)
                .put("first", firstSession).put("second", secondSession).put("restored", true).put("externalDeletion", true).toString())
        }
    }
}

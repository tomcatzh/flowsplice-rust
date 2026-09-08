package io.zxf.flowsplice.pty

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class WorkspaceStoreTest {
    private fun workspace() = JSONObject("""{"version":1,"classMode":true,"identityWanted":true,"homes":[{"id":"home","name":"Home","platform":"linux","relay":"","wanted":true}],"tabs":[{"home":"home","session":"session-id","name":"Shell","mode":"read_only","deleted":false}],"active":{"home":"home","session":"session-id"},"selected":"home","page":"terminal"}""")
    @Test fun rejectsSecretsMalformedModesAndCrossNamespace() {
        for (value in listOf(workspace().put("password", "secret"), workspace().put("identityWanted", "true"), workspace().put("classMode", false), workspace().apply { getJSONArray("tabs").getJSONObject(0).put("mode", "write") }, workspace().apply { getJSONArray("homes").getJSONObject(0).put("name", "x".repeat(65537)) })) {
            assertTrue(runCatching { WorkspaceStore.validate(value, true) }.isFailure)
        }
        assertEquals("read_only", WorkspaceStore.validate(workspace(), true).getJSONArray("tabs").getJSONObject(0).getString("mode"))
    }
    @Test fun atomicRoundTripAndNamespaceIsolation() {
        // Instrumentation executes under the target UID. Use a private test subtree,
        // rather than the separate runner package's inaccessible files directory.
        val target = InstrumentationRegistry.getInstrumentation().targetContext
        val directory = target.cacheDir.resolve("workspace-test-" + java.util.UUID.randomUUID()).apply { mkdirs() }
        val context = object : android.content.ContextWrapper(target) {
            override fun getFilesDir() = directory
        }
        val path = context.filesDir.resolve("pty-workspace-class-v1.json")
        val legacy = context.filesDir.resolve("pty-workspace-legacy-v1.json")
        path.delete(); legacy.delete()
        try {
            WorkspaceStore.save(context, true, workspace())
            assertEquals(workspace().toString(), WorkspaceStore.load(context, true).toString())
            assertNull(WorkspaceStore.load(context, false))
            assertTrue(runCatching { WorkspaceStore.save(context, true, workspace().put("secret", "no")) }.isFailure)
            assertEquals(workspace().toString(), WorkspaceStore.load(context, true).toString())
            path.writeText("broken")
            assertNull(WorkspaceStore.load(context, true))
        } finally { directory.deleteRecursively() }
    }
}

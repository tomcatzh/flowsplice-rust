package io.zxf.flowsplice.pty

import android.content.Context
import android.util.AtomicFile
import org.json.JSONObject

/** Metadata only. Never stores credentials, terminal output or protocol actions. */
internal object WorkspaceStore {
    private fun file(context: Context, classMode: Boolean) = AtomicFile(context.filesDir.resolve(if (classMode) "pty-workspace-class-v1.json" else "pty-workspace-legacy-v1.json"))
    fun validate(value: JSONObject, classMode: Boolean): JSONObject {
        fun keys(obj: JSONObject, vararg allowed: String) { require(obj.keys().asSequence().toSet() == allowed.toSet()) }
        fun string(obj: JSONObject, key: String, max: Int = 128, nullable: Boolean = false) {
            if (nullable && obj.isNull(key)) return
            val text = obj.get(key); require(text is String && text.toByteArray(Charsets.UTF_8).size <= max && text.none { Character.isISOControl(it) })
        }
        fun bool(obj: JSONObject, key: String) { require(obj.get(key) is Boolean) }
        require(value.toString().toByteArray(Charsets.UTF_8).size <= 64 * 1024)
        keys(value, "version", "classMode", "identityWanted", "homes", "tabs", "active", "selected", "page")
        require(value.get("version") == 1 && value.get("classMode") == classMode)
        bool(value, "identityWanted")
        val homes = value.getJSONArray("homes"); require(homes.length() <= 8)
        val ids = mutableSetOf<String>()
        repeat(homes.length()) {
            val home = homes.getJSONObject(it); keys(home, "id", "name", "platform", "relay", "wanted")
            string(home, "id"); string(home, "name"); string(home, "platform", 32); string(home, "relay", 512); bool(home, "wanted"); require(home.getString("platform") in listOf("host", "linux", "macos"))
            require(home.getString("id").isNotEmpty() && ids.add(home.getString("id")))
        }
        val tabs = value.getJSONArray("tabs"); require(tabs.length() <= 64)
        val tabIds = mutableSetOf<Pair<String, String>>()
        repeat(tabs.length()) {
            val tab = tabs.getJSONObject(it); keys(tab, "home", "session", "name", "mode", "deleted")
            string(tab, "home"); string(tab, "session"); string(tab, "name", 256); bool(tab, "deleted")
            require(tab.getString("mode") in listOf("read_only", "read_write"))
            require(tab.getString("home") in ids && tab.getString("session").isNotEmpty())
            require(tabIds.add(tab.getString("home") to tab.getString("session")))
        }
        if (!value.isNull("active")) {
            val active = value.getJSONObject("active"); keys(active, "home", "session"); string(active, "home"); string(active, "session")
            require((active.getString("home") to active.getString("session")) in tabIds)
        }
        string(value, "selected", nullable = true)
        require(value.isNull("selected") || value.getString("selected") in ids)
        require(value.getString("page") in listOf("manage", "terminal", "opened"))
        return value
    }
    fun load(context: Context, classMode: Boolean): JSONObject? = runCatching {
        val bytes = file(context, classMode).openRead().use { it.readNBytes(64 * 1024 + 1) }
        require(bytes.size <= 64 * 1024)
        validate(JSONObject(bytes.toString(Charsets.UTF_8)), classMode)
    }.getOrNull()
    fun save(context: Context, classMode: Boolean, value: JSONObject) {
        val bytes = validate(value, classMode).toString().toByteArray(Charsets.UTF_8)
        val target = file(context, classMode)
        val stream = target.startWrite()
        try { stream.write(bytes); target.finishWrite(stream) } catch (error: Exception) { target.failWrite(stream); throw error }
    }
}

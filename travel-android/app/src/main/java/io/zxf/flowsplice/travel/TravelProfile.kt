package io.zxf.flowsplice.travel

import android.content.Context
import android.net.Uri
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import androidx.core.content.edit
import java.io.File
import java.io.InputStream
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec
import java.util.zip.ZipInputStream

object TravelProfile {
    private const val PROFILE_DIRECTORY = "travel-profile"
    private const val CONFIG_FILE = "travelagent.toml"
    private const val MAX_ENTRIES = 128
    private const val MAX_ENTRY_BYTES = 16L * 1024 * 1024
    private const val MAX_TOTAL_BYTES = 64L * 1024 * 1024

    fun directory(context: Context): File = File(context.filesDir, PROFILE_DIRECTORY)

    fun config(context: Context): File = File(directory(context), CONFIG_FILE)

    fun isInstalled(context: Context): Boolean = config(context).isFile

    fun install(context: Context, source: Uri, password: String) {
        val input = context.contentResolver.openInputStream(source)
            ?: error("Could not open the selected package")
        input.use { install(context, it, password) }
    }

    internal fun install(context: Context, source: InputStream, password: String) {
        require(password.isNotBlank()) { "Private-key password is required" }
        val staging = File(context.filesDir, "$PROFILE_DIRECTORY-installing")
        staging.deleteRecursively()
        check(staging.mkdirs()) { "Could not create the profile staging directory" }
        try {
            extractZip(source, staging)
            val sourceConfig = staging.walkTopDown().firstOrNull { it.isFile && it.name == CONFIG_FILE }
                ?: error("The package does not contain $CONFIG_FILE")
            val sourceRoot = sourceConfig.parentFile ?: error("The profile root is invalid")
            validateRequiredFiles(sourceRoot)

            val next = File(context.filesDir, "$PROFILE_DIRECTORY-next")
            next.deleteRecursively()
            check(sourceRoot.copyRecursively(next)) { "Could not copy the profile" }

            val installed = directory(context)
            val backup = File(context.filesDir, "$PROFILE_DIRECTORY-backup")
            backup.deleteRecursively()
            if (installed.exists()) {
                check(installed.renameTo(backup)) { "Could not preserve the current profile" }
            }
            try {
                check(next.renameTo(installed)) { "Could not activate the imported profile" }
                rewriteConfiguration(installed)
                CredentialStore.save(context, password)
                backup.deleteRecursively()
            } catch (error: Throwable) {
                installed.deleteRecursively()
                if (backup.exists()) backup.renameTo(installed)
                throw error
            }
        } finally {
            staging.deleteRecursively()
        }
    }

    private fun extractZip(source: InputStream, destination: File) {
        val destinationPath = destination.canonicalPath + File.separator
        var entries = 0
        var totalBytes = 0L
        ZipInputStream(source.buffered()).use { zip ->
            while (true) {
                val entry = zip.nextEntry ?: break
                entries += 1
                check(entries <= MAX_ENTRIES) { "The profile package contains too many files" }
                val target = File(destination, entry.name)
                check(target.canonicalPath.startsWith(destinationPath)) { "The package contains an unsafe path" }
                if (entry.isDirectory) {
                    check(target.mkdirs() || target.isDirectory) { "Could not create ${entry.name}" }
                } else {
                    val parent = target.parentFile ?: error("The package contains an invalid path")
                    check(parent.mkdirs() || parent.isDirectory) { "Could not create ${entry.name}" }
                    target.outputStream().buffered().use { output ->
                        val buffer = ByteArray(DEFAULT_BUFFER_SIZE)
                        var entryBytes = 0L
                        while (true) {
                            val count = zip.read(buffer)
                            if (count < 0) break
                            entryBytes += count
                            totalBytes += count
                            check(entryBytes <= MAX_ENTRY_BYTES && totalBytes <= MAX_TOTAL_BYTES) {
                                "The profile package is too large"
                            }
                            output.write(buffer, 0, count)
                        }
                    }
                }
                zip.closeEntry()
            }
        }
    }

    private fun validateRequiredFiles(root: File) {
        val required = listOf(
            "deployment-root.pub",
            "deployment-trust.json",
            "travel-management.crt",
            "travel-management.key",
            "management-ca.crt",
            "travel-business.crt",
            "travel-business.key",
            "business-ca.crt",
        )
        val names = root.walkTopDown().filter(File::isFile).map(File::getName).toSet()
        val missing = required.filterNot(names::contains)
        check(missing.isEmpty()) { "The profile is missing ${missing.joinToString()}" }
    }

    private fun rewriteConfiguration(root: File) {
        val config = File(root, CONFIG_FILE)
        check(config.isFile) { "The profile configuration is missing" }
        val files = root.walkTopDown().filter(File::isFile).associateBy(File::getName)
        var text = config.readText()
        val paths = mapOf(
            "deployment_root_public_key" to "deployment-root.pub",
            "deployment_trust" to "deployment-trust.json",
            "management_cert" to "travel-management.crt",
            "management_key" to "travel-management.key",
            "management_ca" to "management-ca.crt",
            "business_cert" to "travel-business.crt",
            "business_key" to "travel-business.key",
            "business_ca" to "business-ca.crt",
        )
        for ((key, name) in paths) {
            val file = files[name] ?: error("The profile is missing $name")
            text = replaceTomlValue(text, key, file.absolutePath)
        }
        val stateDirectory = File(root, "state").apply { mkdirs() }
        text = replaceTomlValue(text, "state_store", File(stateDirectory, "travel-state.redb").absolutePath)
        text = replaceTomlValue(text, "enrollment_work_dir", File(stateDirectory, "travel-enrollment").absolutePath)
        text = replaceTomlValue(text, "ui_listen", "127.0.0.1:0")
        text = text.lineSequence()
            .filterNot { line ->
                val trimmed = line.trimStart()
                trimmed.startsWith("test_allow_remote_listen") || trimmed.startsWith("test_admin_token")
            }
            .joinToString("\n", postfix = "\n")
        config.writeText(text)
    }

    internal fun replaceTomlValue(text: String, key: String, value: String): String {
        val escaped = value.replace("\\", "\\\\").replace("\"", "\\\"")
        val pattern = Regex("(?m)^${Regex.escape(key)}[ \\t]*=[ \\t]*\"[^\"]*\"[ \\t]*$")
        check(pattern.containsMatchIn(text)) { "The profile configuration is missing $key" }
        return text.replace(pattern, "$key = \"$escaped\"")
    }
}

object CredentialStore {
    private const val KEY_ALIAS = "flowsplice-travel-private-key"
    private const val PREFERENCES = "travel-credentials"
    private const val CIPHERTEXT = "private-key-password"

    fun save(context: Context, password: String) {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, secretKey())
        val encoded = cipher.doFinal(password.toByteArray(Charsets.UTF_8))
        context.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
            .edit {
                putString(CIPHERTEXT, Base64.encodeToString(cipher.iv + encoded, Base64.NO_WRAP))
            }
    }

    fun load(context: Context): String? {
        val value = context.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
            .getString(CIPHERTEXT, null) ?: return null
        val payload = Base64.decode(value, Base64.NO_WRAP)
        if (payload.size <= 12) return null
        return runCatching {
            val cipher = Cipher.getInstance("AES/GCM/NoPadding")
            cipher.init(Cipher.DECRYPT_MODE, secretKey(), GCMParameterSpec(128, payload.copyOfRange(0, 12)))
            cipher.doFinal(payload.copyOfRange(12, payload.size)).toString(Charsets.UTF_8)
        }.getOrNull()
    }

    private fun secretKey(): SecretKey {
        val keyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (keyStore.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        generator.init(
            KeyGenParameterSpec.Builder(
                KEY_ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .build(),
        )
        return generator.generateKey()
    }
}

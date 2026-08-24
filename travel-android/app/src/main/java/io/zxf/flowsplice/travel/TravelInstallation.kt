package io.zxf.flowsplice.travel

import android.content.Context
import android.os.Build
import android.provider.Settings
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import androidx.core.content.edit
import java.io.File
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

object TravelInstallation {
    private const val INSTALL_DIRECTORY = "travel-installation"
    private const val CONFIG_FILE = "travelagent.toml"

    fun directory(context: Context): File = File(context.filesDir, INSTALL_DIRECTORY)

    fun config(context: Context): File = File(directory(context), CONFIG_FILE)

    fun isInstalled(context: Context): Boolean = config(context).isFile

    fun discardPending(context: Context) {
        if (!isInstalled(context)) directory(context).deleteRecursively()
    }
}

object DeviceIdentity {
    fun defaultTravelId(context: Context): String {
        val deviceName = runCatching {
            Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME)
        }.getOrNull()
        return normalizeTravelId(deviceName.orEmpty())
            .ifEmpty { normalizeTravelId(Build.MODEL) }
            .ifEmpty { "android-travel" }
    }

    internal fun normalizeTravelId(value: String): String = value
        .trim()
        .map { character ->
            if (character.isAsciiLetterOrDigit() || character in "._-") character else '-'
        }
        .joinToString("")
        .replace(Regex("-+"), "-")
        .trim('-', '.', '_')
        .take(128)

    private fun Char.isAsciiLetterOrDigit(): Boolean =
        this in 'a'..'z' || this in 'A'..'Z' || this in '0'..'9'
}

object RelayPreference {
    private const val PREFERENCES = "travel-onboarding"
    private const val LAST_RELAY = "last-relay-address"

    fun load(context: Context): String = context
        .getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
        .getString(LAST_RELAY, "")
        .orEmpty()

    fun save(context: Context, value: String) {
        context.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
            .edit { putString(LAST_RELAY, value.trim()) }
    }

    internal fun isValid(value: String): Boolean {
        val address = value.trim()
        val separator = address.lastIndexOf(':')
        if (separator <= 0 || separator == address.lastIndex) return false
        val host = address.substring(0, separator)
        val port = address.substring(separator + 1).toIntOrNull() ?: return false
        if (port !in 1..65535) return false
        return if (host.startsWith('[') || host.endsWith(']')) {
            host.length > 2 && host.startsWith('[') && host.endsWith(']')
        } else {
            !host.contains(':') && host.none(Char::isWhitespace)
        }
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

    fun clear(context: Context) {
        context.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE).edit { clear() }
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

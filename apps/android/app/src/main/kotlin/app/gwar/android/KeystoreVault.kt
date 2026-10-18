package app.gwar.android

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.security.keystore.StrongBoxUnavailableException
import app.gwar.core.KeyVault
import app.gwar.core.VaultException
import java.security.GeneralSecurityException
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Seals the device identity with an AES-256-GCM key kept in the Android
 * Keystore: it never leaves the keystore (StrongBox hardware where the phone
 * has it), so a copy of the app's files cannot be opened elsewhere. The key
 * needs no screen unlock, since a call may reconnect with the screen off.
 *
 * Sealed form: a version byte, the 12-byte nonce, then the ciphertext with its tag.
 */
class KeystoreVault : KeyVault {
    override fun seal(plain: ByteArray): ByteArray = guarded {
        try {
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(Cipher.ENCRYPT_MODE, key())
            byteArrayOf(VERSION) + cipher.iv + cipher.doFinal(plain)
        } finally {
            plain.fill(0)
        }
    }

    override fun open(sealed: ByteArray): ByteArray = guarded {
        if (sealed.size <= 1 + NONCE || sealed[0] != VERSION) throw GeneralSecurityException("unknown sealed format")
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.DECRYPT_MODE, existingKey(), GCMParameterSpec(TAG_BITS, sealed, 1, NONCE))
        cipher.doFinal(sealed, 1 + NONCE, sealed.size - 1 - NONCE)
    }

    private fun keystore() = KeyStore.getInstance(KEYSTORE).apply { load(null) }

    private fun existingKey(): SecretKey =
        keystore().getKey(ALIAS, null) as? SecretKey ?: throw GeneralSecurityException("the identity key is gone")

    private fun key(): SecretKey = keystore().getKey(ALIAS, null) as? SecretKey ?: generate()

    private fun generate(): SecretKey {
        fun spec(strongBox: Boolean) = KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .setIsStrongBoxBacked(strongBox)
            .build()
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE)
        return try {
            generator.init(spec(strongBox = true))
            generator.generateKey()
        } catch (e: StrongBoxUnavailableException) {
            generator.init(spec(strongBox = false))
            generator.generateKey()
        }
    }

    private inline fun guarded(block: () -> ByteArray): ByteArray = try {
        block()
    } catch (e: GeneralSecurityException) {
        throw VaultException.Failed(e.toString())
    }

    companion object {
        private const val KEYSTORE = "AndroidKeyStore"
        private const val ALIAS = "gwar-identity"
        private const val TRANSFORMATION = "AES/GCM/NoPadding"
        private const val VERSION: Byte = 1
        private const val NONCE = 12
        private const val TAG_BITS = 128

        /** Deletes the key; the sealed identity can then never be opened again. */
        fun forget() {
            KeyStore.getInstance(KEYSTORE).apply { load(null) }.deleteEntry(ALIAS)
        }
    }
}

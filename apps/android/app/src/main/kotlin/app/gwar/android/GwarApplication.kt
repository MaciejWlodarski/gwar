package app.gwar.android

import android.app.Application
import app.gwar.core.GwarApp
import java.io.File

class GwarApplication : Application() {
    private val gwarDir get() = File(filesDir, "gwar")
    private var opened: GwarApp? = null

    /**
     * The Rust core: identity, pins and connections. One per process. Throws
     * `AppException.IdentityLocked` when the sealed identity no longer opens.
     */
    fun core(): GwarApp = opened ?: GwarApp(gwarDir.absolutePath, KeystoreVault()).also { opened = it }

    /** The connection the screens show; it outlives them while a call runs. Needs [core]. */
    val client: GwarClient by lazy { GwarClient(this) }

    /** Starts over with a new identity: the sealed one and its key are deleted for good. */
    fun resetIdentity() {
        opened = null
        File(gwarDir, "identity.sealed").delete()
        File(gwarDir, "identity").delete()
        KeystoreVault.forget()
    }
}

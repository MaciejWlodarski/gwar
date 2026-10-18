package app.gwar.android

import android.app.Application
import app.gwar.core.GwarApp
import java.io.File

class GwarApplication : Application() {
    /** The Rust core: identity, pins and connections. One per process. */
    val core: GwarApp by lazy { GwarApp(File(filesDir, "gwar").absolutePath) }

    /** The connection the screens show; it outlives them while a call runs. */
    val client: GwarClient by lazy { GwarClient(this) }
}

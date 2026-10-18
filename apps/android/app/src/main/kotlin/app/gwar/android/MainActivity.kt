package app.gwar.android

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import app.gwar.android.ui.GwarRoot
import app.gwar.android.ui.GwarTheme
import app.gwar.android.ui.LockedScreen
import app.gwar.core.AppException

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val app = application as GwarApplication
        // The reason the identity does not open, or null once the core is open.
        fun open(): AppException? = try {
            app.core()
            null
        } catch (e: AppException) {
            e
        }
        setContent {
            GwarTheme {
                var locked by remember { mutableStateOf(open()) }
                val error = locked
                if (error != null) {
                    LockedScreen(error) {
                        app.resetIdentity()
                        locked = open()
                    }
                } else {
                    GwarRoot(app.client)
                }
            }
        }
    }
}

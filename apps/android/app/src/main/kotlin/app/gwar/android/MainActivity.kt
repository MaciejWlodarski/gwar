package app.gwar.android

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import app.gwar.android.ui.GwarRoot
import app.gwar.android.ui.GwarTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val client = (application as GwarApplication).client
        setContent {
            GwarTheme {
                GwarRoot(client)
            }
        }
    }
}

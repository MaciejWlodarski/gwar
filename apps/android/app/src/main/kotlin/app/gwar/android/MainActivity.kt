package app.gwar.android

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import app.gwar.android.ui.GwarRoot
import app.gwar.android.ui.GwarTheme

class MainActivity : ComponentActivity() {
    private val model: GwarViewModel by viewModels()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            GwarTheme {
                GwarRoot(model)
            }
        }
    }
}

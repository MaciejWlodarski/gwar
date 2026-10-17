package app.gwar.android.ui

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.ReadOnlyComposable
import androidx.compose.ui.graphics.Color

// The palette of apps/web/src/index.css: one neutral ramp and one accent.
private val Dark = darkColorScheme(
    primary = Color(0xFF7482F5),
    onPrimary = Color.White,
    primaryContainer = Color(0x297482F5),
    onPrimaryContainer = Color(0xFFE9EAEE),
    background = Color(0xFF131418),
    onBackground = Color(0xFFE9EAEE),
    surface = Color(0xFF131418),
    onSurface = Color(0xFFE9EAEE),
    surfaceVariant = Color(0xFF1E2026),
    onSurfaceVariant = Color(0xFF9A9EAA),
    surfaceContainer = Color(0xFF17181D),
    surfaceContainerHigh = Color(0xFF1E2026),
    surfaceContainerHighest = Color(0xFF24262D),
    outline = Color(0x21FFFFFF),
    outlineVariant = Color(0x12FFFFFF),
    error = Color(0xFFF2646A),
    onError = Color.White,
    errorContainer = Color(0x24F2646A),
    onErrorContainer = Color(0xFFF2646A),
)

private val Light = lightColorScheme(
    primary = Color(0xFF4F5DE0),
    onPrimary = Color.White,
    primaryContainer = Color(0x1F4F5DE0),
    onPrimaryContainer = Color(0xFF16181D),
    background = Color(0xFFF3F4F7),
    onBackground = Color(0xFF16181D),
    surface = Color(0xFFF3F4F7),
    onSurface = Color(0xFF16181D),
    surfaceVariant = Color(0xFFEEF0F4),
    onSurfaceVariant = Color(0xFF565B68),
    surfaceContainer = Color(0xFFFFFFFF),
    surfaceContainerHigh = Color(0xFFEEF0F4),
    surfaceContainerHighest = Color(0xFFE6E8EE),
    outline = Color(0x290F1428),
    outlineVariant = Color(0x170F1428),
    error = Color(0xFFD8393F),
    onError = Color.White,
    errorContainer = Color(0x1AD8393F),
    onErrorContainer = Color(0xFFD8393F),
)

/** Colours outside Material's roles. */
object GwarColors {
    val talking: Color
        @Composable @ReadOnlyComposable
        get() = if (isSystemInDarkTheme()) Color(0xFF3DDC97) else Color(0xFF17A56B)

    val warn: Color
        @Composable @ReadOnlyComposable
        get() = if (isSystemInDarkTheme()) Color(0xFFF2B84B) else Color(0xFFB97A0A)
}

@Composable
fun GwarTheme(content: @Composable () -> Unit) {
    MaterialTheme(colorScheme = if (isSystemInDarkTheme()) Dark else Light, content = content)
}

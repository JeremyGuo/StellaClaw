package com.stellaclaw.stellacodex.ui.theme

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.ColorScheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable

private val DarkColors = darkColorScheme(
    primary = StellaBlue,
    secondary = StellaGreen,
    background = StellaBackground,
    surface = StellaSurface,
)

private val LightColors = lightColorScheme(
    primary = StellaBlue,
    secondary = StellaGreen,
)

@Composable
fun StellacodeXTheme(
    darkTheme: Boolean = isSystemInDarkTheme(),
    content: @Composable () -> Unit,
) {
    val colors: ColorScheme = if (darkTheme) DarkColors else LightColors

    MaterialTheme(
        colorScheme = colors,
        typography = Typography,
        content = content,
    )
}

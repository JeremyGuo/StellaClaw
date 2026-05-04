package com.stellaclaw.stellacodex

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import com.stellaclaw.stellacodex.ui.app.StellacodeXApp
import com.stellaclaw.stellacodex.ui.chat.AgentCompletionService
import com.stellaclaw.stellacodex.ui.theme.StellacodeXTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        AgentCompletionService.start(this)
        setContent {
            StellacodeXTheme {
                StellacodeXApp()
            }
        }
    }
}

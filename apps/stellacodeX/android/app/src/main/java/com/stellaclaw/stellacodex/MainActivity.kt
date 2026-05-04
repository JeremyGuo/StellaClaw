package com.stellaclaw.stellacodex

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.mutableStateOf
import com.stellaclaw.stellacodex.data.log.AppLogStore
import com.stellaclaw.stellacodex.ui.app.StellacodeXApp
import com.stellaclaw.stellacodex.ui.chat.AgentCompletionService
import com.stellaclaw.stellacodex.ui.theme.StellacodeXTheme

class MainActivity : ComponentActivity() {
    private val requestedConversationId = mutableStateOf<String?>(null)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        AppLogStore.installCrashHandler(applicationContext)
        AppLogStore.append(applicationContext, "app", "MainActivity.onCreate sdk=${android.os.Build.VERSION.SDK_INT}")
        requestedConversationId.value = intent.conversationIdExtra()
        enableEdgeToEdge()
        runCatching {
            AgentCompletionService.start(this)
        }.onFailure { error ->
            AppLogStore.append(applicationContext, "app", "AgentCompletionService.start failed ${error::class.java.simpleName}: ${error.message.orEmpty()}")
        }
        setContent {
            StellacodeXTheme {
                StellacodeXApp(requestedConversationId = requestedConversationId.value)
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        requestedConversationId.value = intent.conversationIdExtra()
        AppLogStore.append(applicationContext, "app", "MainActivity.onNewIntent conversation=${requestedConversationId.value.orEmpty()}")
    }

    private fun Intent?.conversationIdExtra(): String? = this
        ?.getStringExtra(EXTRA_CONVERSATION_ID)
        ?.takeIf { it.isNotBlank() }

    companion object {
        const val EXTRA_CONVERSATION_ID = "com.stellaclaw.stellacodex.extra.CONVERSATION_ID"
    }
}

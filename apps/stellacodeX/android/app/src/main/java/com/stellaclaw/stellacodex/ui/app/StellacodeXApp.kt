package com.stellaclaw.stellacodex.ui.app

import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.ui.platform.LocalContext
import com.stellaclaw.stellacodex.data.log.AppLogStore
import com.stellaclaw.stellacodex.data.update.ApkUpdateDownloader
import com.stellaclaw.stellacodex.ui.navigation.AppNavGraph

@Composable
fun StellacodeXApp(requestedConversationId: String? = null) {
    val context = LocalContext.current.applicationContext
    LaunchedEffect(Unit) {
        AppLogStore.initialize(context)
        ApkUpdateDownloader.cleanupOldApks(context) { message ->
            AppLogStore.append(context, "update", message)
        }
    }
    AppNavGraph(requestedConversationId = requestedConversationId)
}

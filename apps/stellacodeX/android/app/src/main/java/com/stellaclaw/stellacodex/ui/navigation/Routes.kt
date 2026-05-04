package com.stellaclaw.stellacodex.ui.navigation

import android.net.Uri

sealed class AppRoute(val route: String) {
    data object Connections : AppRoute("connections")
    data object Conversations : AppRoute("conversations")
    data object Chat : AppRoute("conversations/{conversationId}") {
        fun create(conversationId: String): String = "conversations/${Uri.encode(conversationId)}"
    }
    data object Workspace : AppRoute("conversations/{conversationId}/workspace?path={path}")
    data object Settings : AppRoute("settings")
    data object Logs : AppRoute("logs")
}

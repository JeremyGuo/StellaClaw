package com.stellaclaw.stellacodex.ui.navigation

sealed class AppRoute(val route: String) {
    data object Connections : AppRoute("connections")
    data object Conversations : AppRoute("conversations")
    data object Chat : AppRoute("conversations/{conversationId}") {
        fun create(conversationId: String): String = "conversations/$conversationId"
    }
    data object Workspace : AppRoute("conversations/{conversationId}/workspace?path={path}")
    data object Settings : AppRoute("settings")
}

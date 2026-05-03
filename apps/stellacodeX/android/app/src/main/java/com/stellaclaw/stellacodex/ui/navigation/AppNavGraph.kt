package com.stellaclaw.stellacodex.ui.navigation

import androidx.compose.runtime.Composable
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import com.stellaclaw.stellacodex.ui.chat.ChatScreen
import com.stellaclaw.stellacodex.ui.connections.ConnectionsScreen
import com.stellaclaw.stellacodex.ui.conversations.ConversationListScreen
import com.stellaclaw.stellacodex.ui.logs.LogsScreen
import com.stellaclaw.stellacodex.ui.settings.SettingsScreen
import com.stellaclaw.stellacodex.ui.workspace.WorkspaceScreen

@Composable
fun AppNavGraph() {
    val navController = rememberNavController()

    NavHost(
        navController = navController,
        startDestination = AppRoute.Connections.route,
    ) {
        composable(AppRoute.Connections.route) {
            ConnectionsScreen(
                onContinue = { navController.navigate(AppRoute.Conversations.route) },
            )
        }
        composable(AppRoute.Conversations.route) {
            ConversationListScreen(
                onOpenConversation = { id -> navController.navigate(AppRoute.Chat.create(id)) },
                onOpenSettings = { navController.navigate(AppRoute.Settings.route) },
                onOpenLogs = { navController.navigate(AppRoute.Logs.route) },
            )
        }
        composable(AppRoute.Chat.route) { backStackEntry ->
            ChatScreen(
                conversationId = backStackEntry.arguments?.getString("conversationId").orEmpty(),
                onBack = { navController.popBackStack() },
                onOpenWorkspace = { conversationId ->
                    navController.navigate("conversations/$conversationId/workspace?path=/")
                },
            )
        }
        composable(AppRoute.Workspace.route) { backStackEntry ->
            WorkspaceScreen(
                conversationId = backStackEntry.arguments?.getString("conversationId").orEmpty(),
                path = backStackEntry.arguments?.getString("path") ?: "/",
                onBack = { navController.popBackStack() },
            )
        }
        composable(AppRoute.Settings.route) {
            SettingsScreen(onBack = { navController.popBackStack() })
        }
        composable(AppRoute.Logs.route) {
            LogsScreen(onBack = { navController.popBackStack() })
        }
    }
}

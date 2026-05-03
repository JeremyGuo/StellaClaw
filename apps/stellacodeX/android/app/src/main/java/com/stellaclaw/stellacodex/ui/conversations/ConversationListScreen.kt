package com.stellaclaw.stellacodex.ui.conversations

import android.app.Application
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import com.stellaclaw.stellacodex.domain.model.ConversationSummary

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ConversationListScreen(
    onOpenConversation: (String) -> Unit,
    onOpenSettings: () -> Unit,
) {
    val application = LocalContext.current.applicationContext as Application
    val viewModel: ConversationListViewModel = viewModel(
        factory = viewModelFactory {
            initializer { ConversationListViewModel(application) }
        },
    )
    val state by viewModel.state.collectAsStateWithLifecycle()

    LaunchedEffect(state.pendingOpenConversationId) {
        val conversationId = state.pendingOpenConversationId ?: return@LaunchedEffect
        viewModel.consumePendingOpenConversation()
        onOpenConversation(conversationId)
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Column {
                        Text("Conversations")
                        Text(
                            text = state.activeConnectionName,
                            style = MaterialTheme.typography.labelSmall,
                        )
                    }
                },
                actions = {
                    TextButton(
                        onClick = viewModel::createConversation,
                        enabled = !state.isCreating,
                    ) { Text(if (state.isCreating) "Creating" else "New") }
                    TextButton(onClick = viewModel::refresh) { Text("Refresh") }
                    TextButton(onClick = onOpenSettings) { Text("Settings") }
                },
            )
        },
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding),
        ) {
            when {
                state.isLoading -> LoadingState()
                state.error != null -> ErrorState(
                    message = state.error.orEmpty(),
                    onRetry = viewModel::refresh,
                )
                state.conversations.isEmpty() -> EmptyState(
                    isCreating = state.isCreating,
                    onCreate = viewModel::createConversation,
                    onRetry = viewModel::refresh,
                )
                else -> ConversationList(
                    conversations = state.conversations,
                    onOpenConversation = onOpenConversation,
                )
            }
        }
    }
}

@Composable
private fun ConversationList(
    conversations: List<ConversationSummary>,
    onOpenConversation: (String) -> Unit,
) {
    LazyColumn(modifier = Modifier.fillMaxSize()) {
        items(conversations, key = { it.conversationId }) { conversation ->
            ConversationRow(
                conversation = conversation,
                onClick = { onOpenConversation(conversation.conversationId) },
            )
            HorizontalDivider()
        }
    }
}

@Composable
private fun LoadingState() {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(24.dp),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        CircularProgressIndicator()
        Text("Loading conversations...")
    }
}

@Composable
private fun ErrorState(message: String, onRetry: () -> Unit) {
    Column(
        modifier = Modifier.padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text(
            text = message,
            color = MaterialTheme.colorScheme.error,
        )
        Button(onClick = onRetry) { Text("Retry") }
    }
}

@Composable
private fun EmptyState(
    isCreating: Boolean,
    onCreate: () -> Unit,
    onRetry: () -> Unit,
) {
    Column(
        modifier = Modifier.padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("No conversations yet.")
        Button(onClick = onCreate, enabled = !isCreating) { Text(if (isCreating) "Creating..." else "Create conversation") }
        Button(onClick = onRetry) { Text("Refresh") }
    }
}

@Composable
private fun ConversationRow(
    conversation: ConversationSummary,
    onClick: () -> Unit,
) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(16.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Column(
            modifier = Modifier.weight(1f),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            Text(text = conversation.displayName, style = MaterialTheme.typography.titleMedium)
            Text(text = conversation.conversationId, style = MaterialTheme.typography.bodySmall)
            if (conversation.model.isNotBlank()) {
                Text(
                    text = "model: ${conversation.model}" + if (conversation.modelSelectionPending) " · selection pending" else "",
                    style = MaterialTheme.typography.labelSmall,
                )
            }
            Text(
                text = listOf(
                    conversation.reasoning.takeIf { it.isNotBlank() }?.let { "reasoning: $it" },
                    conversation.sandbox.takeIf { it.isNotBlank() }?.let { "sandbox: $it" },
                    conversation.remote.takeIf { it.isNotBlank() }?.let { "remote: $it" },
                ).filterNotNull().joinToString(" · "),
                style = MaterialTheme.typography.labelSmall,
            )
            if (conversation.workspace.isNotBlank()) {
                Text(text = "workspace: ${conversation.workspace}", style = MaterialTheme.typography.labelSmall)
            }
            if (conversation.totalBackground > 0 || conversation.totalSubagents > 0) {
                Text(
                    text = "sessions: ${conversation.totalBackground} background · ${conversation.totalSubagents} subagents",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
            }
            conversation.lastMessageTime?.let { time ->
                Text(text = time, style = MaterialTheme.typography.labelSmall)
            }
        }
        Column(horizontalAlignment = Alignment.End) {
            Text(
                text = when {
                    conversation.hasUnread -> "unread"
                    conversation.running -> "running"
                    else -> conversation.processingState
                },
                style = MaterialTheme.typography.labelMedium,
                color = if (conversation.running) {
                    MaterialTheme.colorScheme.primary
                } else {
                    MaterialTheme.colorScheme.onSurfaceVariant
                },
            )
            Text(
                text = "${conversation.messageCount} msgs",
                style = MaterialTheme.typography.labelSmall,
            )
        }
    }
}

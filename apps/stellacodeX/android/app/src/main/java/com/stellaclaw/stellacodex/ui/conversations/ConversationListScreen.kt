package com.stellaclaw.stellacodex.ui.conversations

import android.app.Application
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Article
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Settings
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
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
    onOpenLogs: () -> Unit,
) {
    val application = LocalContext.current.applicationContext as Application
    val viewModel: ConversationListViewModel = viewModel(
        factory = viewModelFactory {
            initializer { ConversationListViewModel(application) }
        },
    )
    val state by viewModel.state.collectAsStateWithLifecycle()
    val lifecycleOwner = LocalLifecycleOwner.current

    DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME) {
                viewModel.refreshOnResume()
            }
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose { lifecycleOwner.lifecycle.removeObserver(observer) }
    }

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
                    IconButton(
                        onClick = viewModel::createConversation,
                        enabled = !state.isCreating,
                    ) {
                        Icon(Icons.Filled.Add, contentDescription = if (state.isCreating) "Creating" else "New conversation")
                    }
                    IconButton(onClick = viewModel::refresh) {
                        Icon(Icons.Filled.Refresh, contentDescription = "Refresh")
                    }
                    IconButton(onClick = onOpenLogs) {
                        Icon(Icons.Filled.Article, contentDescription = "Logs")
                    }
                    IconButton(onClick = onOpenSettings) {
                        Icon(Icons.Filled.Settings, contentDescription = "Settings")
                    }
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
            ConversationStatusIndicator(conversation = conversation)
            Text(
                text = "${conversation.messageCount} msgs",
                style = MaterialTheme.typography.labelSmall,
            )
        }
    }
}

@Composable
private fun ConversationStatusIndicator(conversation: ConversationSummary) {
    Box(
        modifier = Modifier.size(24.dp),
        contentAlignment = Alignment.Center,
    ) {
        when {
            conversation.running -> CircularProgressIndicator(
                modifier = Modifier.size(18.dp),
                strokeWidth = 2.dp,
            )
            conversation.hasUnread -> Box(
                modifier = Modifier
                    .size(10.dp)
                    .background(MaterialTheme.colorScheme.primary, CircleShape),
            )
        }
    }
}

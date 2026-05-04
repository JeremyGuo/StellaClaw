package com.stellaclaw.stellacodex.ui.conversations

import android.Manifest
import android.app.Application
import android.os.Build
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Article
import androidx.compose.material.icons.filled.AutoAwesome
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import com.stellaclaw.stellacodex.domain.model.ConversationSummary
import com.stellaclaw.stellacodex.ui.chat.AgentNotificationCenter
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter

private val ListBackground = Color(0xFFF4F4F7)
private val ListSurface = Color(0xF6FFFFFF)
private val ListBorder = Color(0x1A000000)
private val ListMutedText = Color(0xFF8E8E93)
private val OnlineGreen = Color(0xFF30D681)
private val UnreadRed = Color(0xFFFF453A)
private val AvatarBlue = Color(0xFF0A95FF)
private val AvatarPurple = Color(0xFF7A45FF)

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
    val notificationPermissionLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { }

    LaunchedEffect(Unit) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU && !AgentNotificationCenter.canNotify(application)) {
            notificationPermissionLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
    }

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

    Scaffold(containerColor = ListBackground) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .background(ListBackground),
        ) {
            ConversationListHeader(
                connectionName = state.activeConnectionName,
                isCreating = state.isCreating,
                onCreate = viewModel::createConversation,
                onOpenSettings = onOpenSettings,
                onOpenLogs = onOpenLogs,
            )
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
private fun ConversationListHeader(
    connectionName: String,
    isCreating: Boolean,
    onCreate: () -> Unit,
    onOpenSettings: () -> Unit,
    onOpenLogs: () -> Unit,
) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 22.dp, vertical = 18.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Row(
            horizontalArrangement = Arrangement.spacedBy(14.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Surface(
                modifier = Modifier.size(58.dp),
                shape = CircleShape,
                color = AvatarPurple,
            ) {
                Box(contentAlignment = Alignment.Center) {
                    Icon(Icons.Filled.AutoAwesome, contentDescription = null, tint = Color.White)
                }
            }
            Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Text(
                    text = "StellaClaw",
                    style = MaterialTheme.typography.headlineMedium,
                    fontWeight = FontWeight.Bold,
                )
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(7.dp)) {
                    Box(
                        modifier = Modifier
                            .size(12.dp)
                            .clip(CircleShape)
                            .background(OnlineGreen),
                    )
                    Text("在线 - $connectionName ›", style = MaterialTheme.typography.bodyLarge)
                }
            }
        }
        Row(verticalAlignment = Alignment.CenterVertically) {
            IconButton(onClick = onOpenLogs) {
                Icon(Icons.Filled.Article, contentDescription = "Logs")
            }
            IconButton(onClick = onOpenSettings) {
                Icon(Icons.Filled.Settings, contentDescription = "Settings")
            }
            IconButton(onClick = onCreate, enabled = !isCreating) {
                Icon(
                    Icons.Filled.Add,
                    contentDescription = if (isCreating) "Creating" else "New conversation",
                    modifier = Modifier.size(36.dp),
                    tint = Color(0xFF111111),
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
    LazyColumn(
        modifier = Modifier.fillMaxSize(),
        contentPadding = PaddingValues(horizontal = 18.dp, vertical = 8.dp),
        verticalArrangement = Arrangement.spacedBy(18.dp),
    ) {
        items(conversations, key = { it.conversationId }) { conversation ->
            ConversationRow(
                conversation = conversation,
                onClick = { onOpenConversation(conversation.conversationId) },
            )
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
            .clickable(onClick = onClick),
        horizontalArrangement = Arrangement.spacedBy(16.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        ConversationAvatar(conversation)
        Column(
            modifier = Modifier.weight(1f),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    text = conversation.displayName.ifBlank { conversation.conversationId },
                    modifier = Modifier.weight(1f),
                    style = MaterialTheme.typography.headlineSmall,
                    fontWeight = FontWeight.Bold,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    text = formatConversationTime(conversation.lastMessageTime),
                    style = MaterialTheme.typography.bodyMedium,
                    color = ListMutedText.copy(alpha = 0.65f),
                    maxLines = 1,
                )
            }
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    text = conversationPreview(conversation),
                    modifier = Modifier.weight(1f),
                    style = MaterialTheme.typography.titleMedium,
                    color = ListMutedText,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                ConversationStatusIndicator(conversation = conversation)
            }
        }
    }
}

@Composable
private fun ConversationAvatar(conversation: ConversationSummary) {
    Box(modifier = Modifier.size(74.dp), contentAlignment = Alignment.TopEnd) {
        Surface(
            modifier = Modifier
                .size(68.dp)
                .align(Alignment.CenterStart),
            shape = RoundedCornerShape(20.dp),
            color = avatarColor(conversation.conversationId),
            border = BorderStroke(1.dp, Color.White.copy(alpha = 0.7f)),
        ) {
            Box(contentAlignment = Alignment.Center) {
                Text(
                    text = avatarText(conversation.displayName.ifBlank { conversation.conversationId }),
                    style = MaterialTheme.typography.headlineMedium,
                    fontWeight = FontWeight.Bold,
                    color = Color.White,
                )
            }
        }
        if (conversation.hasUnread) {
            Surface(
                modifier = Modifier.size(26.dp),
                shape = CircleShape,
                color = UnreadRed,
            ) {
                Box(contentAlignment = Alignment.Center) {
                    Text("•", color = Color.White, fontWeight = FontWeight.Bold)
                }
            }
        }
    }
}

@Composable
private fun ConversationStatusIndicator(conversation: ConversationSummary) {
    Box(
        modifier = Modifier.size(26.dp),
        contentAlignment = Alignment.Center,
    ) {
        when {
            conversation.running -> CircularProgressIndicator(
                modifier = Modifier.size(18.dp),
                strokeWidth = 2.dp,
                color = OnlineGreen,
            )
            conversation.hasUnread -> Box(
                modifier = Modifier
                    .size(10.dp)
                    .background(UnreadRed, CircleShape),
            )
        }
    }
}

private fun conversationPreview(conversation: ConversationSummary): String = when {
    conversation.running -> "Assistant 正在处理 · ${conversation.processingState}"
    conversation.modelSelectionPending -> "请选择模型后继续"
    conversation.messageCount > 0 -> "${conversation.messageCount} 条消息 · ${conversation.model.ifBlank { "default model" }}"
    else -> "新的会话，点击开始聊天"
}

private fun avatarText(value: String): String = value.trim().take(1).ifBlank { "S" }.uppercase()

private fun avatarColor(seed: String): Color {
    val colors = listOf(AvatarBlue, AvatarPurple, Color(0xFF20C997), Color(0xFFFF9F0A), Color(0xFF5856D6))
    val index = seed.fold(0) { acc, c -> acc + c.code }.let { kotlin.math.abs(it) % colors.size }
    return colors[index]
}

private fun formatConversationTime(value: String?): String {
    if (value.isNullOrBlank()) return ""
    return runCatching {
        Instant.parse(value)
            .atZone(ZoneId.systemDefault())
            .format(ConversationTimeFormatter)
    }.getOrElse { value.take(16) }
}

private val ConversationTimeFormatter: DateTimeFormatter = DateTimeFormatter.ofPattern("HH:mm")

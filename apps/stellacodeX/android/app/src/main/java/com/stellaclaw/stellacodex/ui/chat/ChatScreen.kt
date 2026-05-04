package com.stellaclaw.stellacodex.ui.chat

import android.Manifest
import android.app.Application
import android.os.Build
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.Image
import androidx.compose.foundation.BorderStroke
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
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.filled.AutoAwesome
import androidx.compose.material.icons.filled.AttachFile
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Code
import androidx.compose.material.icons.filled.ContentCopy
import androidx.compose.material.icons.filled.ExpandLess
import androidx.compose.material.icons.filled.ExpandMore
import androidx.compose.material.icons.filled.Folder
import androidx.compose.material.icons.filled.Info
import androidx.compose.material.icons.filled.MoreHoriz
import androidx.compose.material.icons.filled.Terminal
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import com.stellaclaw.stellacodex.domain.model.ChatMessage
import com.stellaclaw.stellacodex.domain.model.ConversationSummary
import com.stellaclaw.stellacodex.domain.model.MessageAttachment
import com.stellaclaw.stellacodex.domain.model.MessageItem
import com.stellaclaw.stellacodex.domain.model.MessageLocalState
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter

private val ChatBackground = Color(0xFFF4F4F7)
private val FrostedSurface = Color(0xF6FFFFFF)
private val FrostedBorder = Color(0x1A000000)
private val MutedText = Color(0xFF8E8E93)
private val UserBubble = Color(0xFF0A95FF)
private val UserBubbleText = Color.White
private val AssistantAccent = Color(0xFF7A45FF)
private val AssistantAccent2 = Color(0xFF2C7BFF)
private val CodeHeaderText = Color(0xFF8A8F98)

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChatScreen(
    conversationId: String,
    onBack: () -> Unit,
    onOpenWorkspace: (String) -> Unit,
) {
    val application = LocalContext.current.applicationContext as Application
    val viewModel: ChatViewModel = viewModel(
        factory = viewModelFactory {
            initializer { ChatViewModel(application) }
        },
    )
    val state by viewModel.state.collectAsStateWithLifecycle()
    val listState = rememberLazyListState()
    var initialBottomPlaced by remember(conversationId) { mutableStateOf(false) }
    var earlierLoadAnchor by remember(conversationId) { mutableStateOf<ScrollAnchor?>(null) }
    var showDetails by remember(conversationId) { mutableStateOf(false) }
    val visibleMessages = remember(state.messages) { state.messages.filterNot(ChatMessage::isRuntimeMetadataMessage) }
    val timeline = remember(visibleMessages) { buildChatTimeline(visibleMessages) }
    val notificationPermissionLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { }

    LaunchedEffect(Unit) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU && !AgentNotificationCenter.canNotify(application)) {
            notificationPermissionLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
    }

    LaunchedEffect(conversationId) {
        initialBottomPlaced = false
        AgentNotificationCenter.dismissConversation(application, conversationId)
        viewModel.load(conversationId)
    }

    LaunchedEffect(timeline.lastOrNull()?.key) {
        if (timeline.isNotEmpty() && earlierLoadAnchor == null) {
            if (!initialBottomPlaced) {
                listState.scrollToItem(timeline.lastIndex)
                initialBottomPlaced = true
            } else {
                listState.animateScrollToItem(timeline.lastIndex)
            }
        }
    }

    LaunchedEffect(timeline, state.isLoadingEarlier, earlierLoadAnchor) {
        val anchor = earlierLoadAnchor ?: return@LaunchedEffect
        if (!state.isLoadingEarlier && timeline.isNotEmpty()) {
            val index = timeline.indexOfFirst { it.key == anchor.key }
            if (index >= 0) {
                listState.scrollToItem(index, anchor.scrollOffset)
            }
            earlierLoadAnchor = null
        }
    }

    LaunchedEffect(
        listState.firstVisibleItemIndex,
        state.loadedOffset,
        state.isLoadingEarlier,
        state.isLoading,
        timeline.isNotEmpty(),
    ) {
        if (initialBottomPlaced &&
            timeline.isNotEmpty() &&
            listState.firstVisibleItemIndex == 0 &&
            state.loadedOffset > 0 &&
            !state.isLoadingEarlier &&
            !state.isLoading
        ) {
            val anchorItem = timeline.getOrNull(listState.firstVisibleItemIndex)
            if (anchorItem != null) {
                earlierLoadAnchor = ScrollAnchor(anchorItem.key, listState.firstVisibleItemScrollOffset)
            }
            viewModel.loadEarlier()
        }
    }

    Scaffold(
        containerColor = ChatBackground,
        topBar = {
            ChatHeader(
                title = state.displayName.ifBlank { conversationId.ifBlank { "Conversation" } },
                subtitle = state.realtimeState.ifBlank { "Conversation stream" },
                onBack = onBack,
                onRefresh = viewModel::refresh,
                onOpenWorkspace = { onOpenWorkspace(conversationId) },
                onShowDetails = { showDetails = true },
            )
        },
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .background(ChatBackground)
                .padding(horizontal = 12.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            state.error?.let { message ->
                Text(
                    text = message,
                    color = MaterialTheme.colorScheme.error,
                    style = MaterialTheme.typography.bodyMedium,
                )
            }
            when {
                state.isLoading && state.messages.isEmpty() -> LoadingMessages()
                visibleMessages.isEmpty() -> EmptyMessages()
                else -> MessageList(
                    isLoadingEarlier = state.isLoadingEarlier,
                    timeline = timeline,
                    listState = listState,
                    previews = state.attachmentPreviews,
                    onPreviewAttachment = viewModel::previewAttachment,
                    onRetrySend = viewModel::retrySend,
                    modifier = Modifier.weight(1f),
                )
            }

            RealtimeStatus(
                realtimeState = state.realtimeState,
                progressTitle = state.progressTitle,
                progressDetail = state.progressDetail,
                progressImportant = state.progressImportant,
            )

            Composer(
                draft = state.draft,
                pendingAttachments = state.pendingAttachments,
                isSending = state.isSending,
                onDraftChanged = viewModel::onDraftChanged,
                onAddAttachments = viewModel::addAttachments,
                onRemoveAttachment = viewModel::removeAttachment,
                onSend = viewModel::send,
            )
        }
    }
    if (showDetails) {
        ConversationDetailsDialog(
            conversationId = conversationId,
            totalMessages = state.totalMessages,
            realtimeState = state.realtimeState,
            summary = state.conversationSummary,
            onDismiss = { showDetails = false },
        )
    }
}

@Composable
private fun ChatHeader(
    title: String,
    subtitle: String,
    onBack: () -> Unit,
    onRefresh: () -> Unit,
    onOpenWorkspace: () -> Unit,
    onShowDetails: () -> Unit,
) {
    Surface(
        color = ChatBackground,
        tonalElevation = 0.dp,
        shadowElevation = 0.dp,
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 12.dp, vertical = 10.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            GlassIconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
            }
            Surface(
                modifier = Modifier.weight(1f),
                shape = RoundedCornerShape(28.dp),
                color = FrostedSurface,
                border = BorderStroke(1.dp, FrostedBorder),
            ) {
                Column(
                    modifier = Modifier.padding(horizontal = 18.dp, vertical = 9.dp),
                    horizontalAlignment = Alignment.CenterHorizontally,
                ) {
                    Text(
                        text = title,
                        style = MaterialTheme.typography.titleMedium,
                        fontWeight = FontWeight.Bold,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    Text(
                        text = subtitle,
                        style = MaterialTheme.typography.bodySmall,
                        color = MutedText,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
            Surface(
                shape = RoundedCornerShape(28.dp),
                color = FrostedSurface,
                border = BorderStroke(1.dp, FrostedBorder),
            ) {
                Row(modifier = Modifier.padding(horizontal = 4.dp, vertical = 4.dp)) {
                    IconButton(onClick = onOpenWorkspace) {
                        Icon(Icons.Filled.Folder, contentDescription = "Files")
                    }
                    IconButton(onClick = onRefresh) {
                        Icon(Icons.Filled.Terminal, contentDescription = "Refresh stream")
                    }
                    IconButton(onClick = onShowDetails) {
                        Icon(Icons.Filled.Info, contentDescription = "Conversation details")
                    }
                    IconButton(onClick = onRefresh) {
                        Icon(Icons.Filled.MoreHoriz, contentDescription = "More")
                    }
                }
            }
        }
    }
}

@Composable
private fun GlassIconButton(
    onClick: () -> Unit,
    content: @Composable () -> Unit,
) {
    Surface(
        modifier = Modifier.size(52.dp),
        shape = CircleShape,
        color = FrostedSurface,
        border = BorderStroke(1.dp, FrostedBorder),
        onClick = onClick,
    ) {
        Box(contentAlignment = Alignment.Center) { content() }
    }
}

@Composable
private fun ConversationDetailsDialog(
    conversationId: String,
    totalMessages: Int,
    realtimeState: String,
    summary: ConversationSummary?,
    onDismiss: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        confirmButton = {
            TextButton(onClick = onDismiss) { Text("关闭") }
        },
        title = { Text("会话详情", fontWeight = FontWeight.Bold) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                DetailLine("名称", summary?.displayName?.takeIf(String::isNotBlank) ?: conversationId)
                DetailLine("会话 ID", conversationId)
                DetailLine("模型", summary?.model?.takeIf(String::isNotBlank) ?: "未知")
                if (summary?.modelSelectionPending == true) {
                    DetailLine("模型状态", "等待选择")
                }
                DetailLine("消息", "${summary?.messageCount ?: totalMessages} msgs")
                DetailLine("实时", realtimeState.ifBlank { "Conversation stream" })
                summary?.reasoning?.takeIf(String::isNotBlank)?.let { DetailLine("Reasoning", it) }
                summary?.sandbox?.takeIf(String::isNotBlank)?.let { DetailLine("Sandbox", it) }
                summary?.remote?.takeIf(String::isNotBlank)?.let { DetailLine("Remote", it) }
                summary?.workspace?.takeIf(String::isNotBlank)?.let { DetailLine("Workspace", it) }
                summary?.let {
                    if (it.totalBackground > 0 || it.totalSubagents > 0) {
                        DetailLine("Sessions", "${it.totalBackground} background · ${it.totalSubagents} subagents")
                    }
                    it.lastMessageTime?.let { time -> DetailLine("最近消息", time) }
                    it.foregroundSessionId.takeIf(String::isNotBlank)?.let { session -> DetailLine("Foreground session", session) }
                }
            }
        },
        containerColor = FrostedSurface,
        shape = RoundedCornerShape(24.dp),
    )
}

@Composable
private fun DetailLine(label: String, value: String) {
    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
        Text(label, style = MaterialTheme.typography.labelMedium, color = MutedText)
        Text(value, style = MaterialTheme.typography.bodyMedium)
    }
}

@Composable
private fun RealtimeStatus(
    realtimeState: String,
    progressTitle: String?,
    progressDetail: String?,
    progressImportant: Boolean,
) {
    if (realtimeState.isBlank() && progressTitle == null) return
    Surface(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 4.dp),
        shape = RoundedCornerShape(18.dp),
        color = FrostedSurface,
        border = BorderStroke(1.dp, FrostedBorder),
    ) {
        Row(
            modifier = Modifier.padding(horizontal = 14.dp, vertical = 12.dp),
            horizontalArrangement = Arrangement.spacedBy(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(
                Icons.Filled.Info,
                contentDescription = null,
                tint = MutedText,
                modifier = Modifier.size(22.dp),
            )
            Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                Text(
                    text = progressTitle?.let { if (progressImportant) "! $it" else it } ?: "Status",
                    style = MaterialTheme.typography.titleSmall,
                    fontWeight = FontWeight.Bold,
                )
                val detail = listOfNotNull(
                    realtimeState.takeIf { it.isNotBlank() },
                    progressDetail?.takeIf { it.isNotBlank() },
                ).joinToString(" · ")
                if (detail.isNotBlank()) {
                    Text(
                        text = detail,
                        style = MaterialTheme.typography.bodyMedium,
                        color = if (detail.contains("error", ignoreCase = true) ||
                            detail.contains("unavailable", ignoreCase = true)
                        ) {
                            MaterialTheme.colorScheme.error
                        } else {
                            MutedText
                        },
                    )
                }
            }
        }
    }
}

@Composable
private fun LoadingMessages() {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(24.dp),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        CircularProgressIndicator()
        Text("Loading messages...")
    }
}

@Composable
private fun EmptyMessages() {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text("No messages yet.")
        Text(
            text = "Send a message to start this conversation.",
            style = MaterialTheme.typography.bodySmall,
        )
    }
}

@Composable
private fun MessageList(
    isLoadingEarlier: Boolean,
    timeline: List<ChatTimelineItem>,
    listState: LazyListState,
    previews: Map<String, AttachmentPreviewUiState>,
    onPreviewAttachment: (MessageAttachment) -> Unit,
    onRetrySend: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    LazyColumn(
        modifier = modifier.fillMaxWidth(),
        state = listState,
        contentPadding = PaddingValues(vertical = 10.dp),
        verticalArrangement = Arrangement.spacedBy(18.dp),
    ) {
        if (isLoadingEarlier) {
            item(key = "loading-earlier") {
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(12.dp),
                    horizontalArrangement = Arrangement.Center,
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    CircularProgressIndicator()
                    Text(
                        text = "Loading earlier messages...",
                        modifier = Modifier.padding(start = 12.dp),
                        style = MaterialTheme.typography.labelSmall,
                    )
                }
            }
        }
        items(timeline, key = { it.key }) { item ->
            when (item) {
                is ChatTimelineItem.Message -> MessageCard(
                    message = item.message,
                    previews = previews,
                    onPreviewAttachment = onPreviewAttachment,
                    onRetrySend = onRetrySend,
                )
                is ChatTimelineItem.ToolSummary -> ToolSummaryCard(summary = item)
            }
        }
    }
}

private data class ScrollAnchor(
    val key: String,
    val scrollOffset: Int,
)

private sealed interface ChatTimelineItem {
    val key: String

    data class Message(val message: ChatMessage) : ChatTimelineItem {
        override val key: String = "message:${message.id}"
    }

    data class ToolSummary(val messages: List<ChatMessage>) : ChatTimelineItem {
        override val key: String = "tools:${messages.first().id}:${messages.last().id}"
        val toolCallCount: Int = messages.sumOf { message -> message.items.count { it is MessageItem.ToolCall } }
        val toolResultCount: Int = messages.sumOf { message -> message.items.count { it is MessageItem.ToolResult } }
    }
}

private fun buildChatTimeline(messages: List<ChatMessage>): List<ChatTimelineItem> {
    val output = mutableListOf<ChatTimelineItem>()
    val pendingTools = mutableListOf<ChatMessage>()
    fun flushPendingTools(fold: Boolean = true) {
        if (pendingTools.isEmpty()) return
        if (fold) {
            output += ChatTimelineItem.ToolSummary(pendingTools.toList())
        } else {
            output += pendingTools.map { ChatTimelineItem.Message(it) }
        }
        pendingTools.clear()
    }

    messages.forEach { message ->
        when {
            message.isToolOnlyMessage() -> pendingTools += message
            else -> {
                flushPendingTools()
                output += ChatTimelineItem.Message(message)
            }
        }
    }
    // Only fold tool-only runs once a following non-tool message closes the chain.
    // A trailing tool run means the assistant turn is still in progress, so keep each tool message visible.
    flushPendingTools(fold = false)
    return output
}

private fun ChatMessage.isRuntimeMetadataMessage(): Boolean {
    val body = text.ifBlank { preview }.trimStart()
    return body.startsWith("[Incoming User Metadata]") ||
        body.startsWith("[Incoming Assistant Metadata]") ||
        body.startsWith("[Incoming System Metadata]")
}

private fun ChatMessage.isToolOnlyMessage(): Boolean =
    role.equals("assistant", ignoreCase = true) &&
        text.isBlank() &&
        attachments.isEmpty() &&
        items.any { it is MessageItem.ToolCall || it is MessageItem.ToolResult }

@Composable
private fun ToolSummaryCard(summary: ChatTimelineItem.ToolSummary) {
    var expanded by remember(summary.key) { mutableStateOf(false) }
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .clickable { expanded = !expanded }
                .padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    text = "Tools · ran ${summary.toolCallCount} commands",
                    style = MaterialTheme.typography.labelMedium,
                    fontWeight = FontWeight.SemiBold,
                )
                Text(
                    text = if (expanded) "Hide list" else "Show list",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
            }
            Text(
                text = "${summary.messages.size} tool messages · ${summary.toolResultCount} results",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            if (expanded) {
                Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    summary.messages.forEach { message ->
                        Text(
                            text = "#${message.index}",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                        ToolItemList(items = message.items)
                    }
                }
            }
        }
    }
}

@Composable
private fun MessageCard(
    message: ChatMessage,
    previews: Map<String, AttachmentPreviewUiState>,
    onPreviewAttachment: (MessageAttachment) -> Unit,
    onRetrySend: (String) -> Unit,
) {
    val isUserMessage = message.role.equals("user", ignoreCase = true)
    val roleLabel = when (message.role.lowercase()) {
        "user" -> message.userName?.takeIf { it.isNotBlank() } ?: "User"
        "assistant" -> "Assistant"
        "system" -> "System"
        else -> message.role.ifBlank { "Message" }
    }
    val toolExplanations = message.items
        .filterIsInstance<MessageItem.ToolCall>()
        .mapNotNull { it.explanation?.trim()?.takeIf(String::isNotEmpty) }
    val displayText = message.text.ifBlank {
        toolExplanations.joinToString("\n\n").ifBlank { message.preview }
    }
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = if (isUserMessage) Arrangement.End else Arrangement.Start,
        verticalAlignment = Alignment.Top,
    ) {
        if (!isUserMessage) {
            AssistantAvatar()
            Spacer(modifier = Modifier.size(10.dp))
        }
        Column(
            modifier = if (isUserMessage) Modifier.fillMaxWidth(0.82f) else Modifier.weight(1f),
            horizontalAlignment = if (isUserMessage) Alignment.End else Alignment.Start,
            verticalArrangement = Arrangement.spacedBy(5.dp),
        ) {
            if (isUserMessage) {
                Row(
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        text = roleLabel,
                        style = MaterialTheme.typography.labelMedium,
                        color = MutedText,
                        fontWeight = FontWeight.SemiBold,
                    )
                    Box(
                        modifier = Modifier
                            .size(6.dp)
                            .clip(CircleShape)
                            .background(MutedText),
                    )
                }
                Surface(
                    shape = RoundedCornerShape(22.dp),
                    color = UserBubble,
                ) {
                    SelectionContainer {
                        Text(
                            text = displayText.ifBlank { " " },
                            modifier = Modifier.padding(horizontal = 18.dp, vertical = 12.dp),
                            style = MaterialTheme.typography.titleMedium,
                            color = UserBubbleText,
                        )
                    }
                }
            } else {
                Text(
                    text = roleLabel,
                    style = MaterialTheme.typography.titleSmall,
                    fontWeight = FontWeight.Bold,
                    color = MutedText,
                )
                if (displayText.isNotBlank()) {
                    SelectionContainer {
                        MessageBody(text = displayText)
                    }
                }
            }
            if (message.items.any { it is MessageItem.ToolCall || it is MessageItem.ToolResult }) {
                ToolItemList(items = message.items)
            }
            if (message.attachments.isNotEmpty()) {
                AttachmentList(
                    attachments = message.attachments,
                    previews = previews,
                    onPreviewAttachment = onPreviewAttachment,
                )
            }
            MessageMetaRow(
                message = message,
                alignEnd = isUserMessage,
                onRetrySend = onRetrySend,
            )
        }
    }
}

@Composable
private fun AssistantAvatar() {
    Box(
        modifier = Modifier
            .size(46.dp)
            .clip(CircleShape)
            .background(AssistantAccent),
        contentAlignment = Alignment.Center,
    ) {
        Box(
            modifier = Modifier
                .size(46.dp)
                .background(AssistantAccent2.copy(alpha = 0.45f)),
        )
        Icon(
            Icons.Filled.AutoAwesome,
            contentDescription = null,
            tint = Color.White,
            modifier = Modifier.size(26.dp),
        )
    }
}

@Composable
private fun MessageMetaRow(
    message: ChatMessage,
    alignEnd: Boolean,
    onRetrySend: (String) -> Unit,
) {
    val usage = message.tokenUsage
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = if (alignEnd) Arrangement.End else Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
            message.messageTime?.let { time ->
                Text(text = formatLocalMinute(time), style = MaterialTheme.typography.labelSmall, color = MutedText)
            }
            when (message.localState) {
                MessageLocalState.Sending -> Text(
                    text = "sending...",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
                MessageLocalState.Failed -> {
                    Text(
                        text = "send failed",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.error,
                    )
                    TextButton(onClick = { onRetrySend(message.id) }) {
                        Text("Retry")
                    }
                }
                MessageLocalState.Synced -> Unit
            }
        }
        if (!alignEnd && (usage != null || message.hasTokenUsage)) {
            Surface(
                shape = RoundedCornerShape(16.dp),
                color = FrostedSurface,
                border = BorderStroke(1.dp, FrostedBorder),
            ) {
                Row(
                    modifier = Modifier.padding(horizontal = 10.dp, vertical = 5.dp),
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Box(
                        modifier = Modifier
                            .size(8.dp)
                            .clip(CircleShape)
                            .background(if (usage != null) Color(0xFF34C759) else Color(0xFFFF3B30)),
                    )
                    Text(
                        text = usage?.let { "${formatCompactNumber(it.total)} tokens" } ?: "usage",
                        style = MaterialTheme.typography.labelLarge,
                        color = MutedText,
                        fontWeight = FontWeight.Bold,
                    )
                }
            }
        }
    }
}

@Composable
private fun MessageBody(text: String) {
    val blocks = markdownBlocks(text)
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        blocks.forEach { block ->
            when (block) {
                is MarkdownBlock.Code -> CodeBlock(block)
                is MarkdownBlock.Text -> MarkdownText(block.text)
            }
        }
    }
}

@Composable
private fun MarkdownText(text: String) {
    Column(verticalArrangement = Arrangement.spacedBy(3.dp)) {
        text.lines().forEach { rawLine ->
            val line = rawLine.trimEnd()
            when {
                line.isBlank() -> Text("", style = MaterialTheme.typography.bodySmall)
                line.startsWith("### ") -> Text(
                    text = line.removePrefix("### "),
                    style = MaterialTheme.typography.titleSmall,
                    fontWeight = FontWeight.SemiBold,
                )
                line.startsWith("## ") -> Text(
                    text = line.removePrefix("## "),
                    style = MaterialTheme.typography.titleMedium,
                    fontWeight = FontWeight.SemiBold,
                )
                line.startsWith("# ") -> Text(
                    text = line.removePrefix("# "),
                    style = MaterialTheme.typography.titleLarge,
                    fontWeight = FontWeight.SemiBold,
                )
                line.startsWith("- ") || line.startsWith("* ") -> Text(
                    text = "• ${line.drop(2)}",
                    style = MaterialTheme.typography.bodyMedium,
                )
                line.matches(Regex("\\d+\\.\\s+.*")) -> Text(
                    text = line,
                    style = MaterialTheme.typography.bodyMedium,
                )
                else -> Text(text = line, style = MaterialTheme.typography.bodyMedium)
            }
        }
    }
}

@Composable
private fun CodeBlock(block: MarkdownBlock.Code) {
    Surface(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 2.dp),
        shape = RoundedCornerShape(14.dp),
        color = FrostedSurface,
        border = BorderStroke(1.dp, FrostedBorder),
    ) {
        Column {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .background(Color.White.copy(alpha = 0.55f))
                    .padding(horizontal = 12.dp, vertical = 9.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Row(
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Icon(
                        Icons.Filled.Code,
                        contentDescription = null,
                        tint = CodeHeaderText,
                        modifier = Modifier.size(20.dp),
                    )
                    Text(
                        text = block.language.ifBlank { "text" },
                        style = MaterialTheme.typography.titleSmall,
                        color = CodeHeaderText,
                        fontWeight = FontWeight.Bold,
                    )
                    Text(
                        text = "${block.code.lines().size} lines",
                        style = MaterialTheme.typography.bodySmall,
                        color = CodeHeaderText.copy(alpha = 0.65f),
                    )
                }
                Row {
                    Icon(Icons.Filled.ExpandLess, contentDescription = "Collapse", tint = CodeHeaderText)
                    Icon(Icons.Filled.ContentCopy, contentDescription = "Copy", tint = CodeHeaderText)
                }
            }
            Text(
                text = block.code.ifBlank { " " },
                modifier = Modifier
                    .fillMaxWidth()
                    .background(Color(0xFFF0F0F4))
                    .padding(12.dp),
                style = MaterialTheme.typography.bodyLarge,
                fontFamily = FontFamily.Monospace,
                color = Color(0xFF101014),
            )
        }
    }
}

@Composable
private fun ToolItemList(items: List<MessageItem>) {
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        items.forEach { item ->
            when (item) {
                is MessageItem.ToolCall -> ToolCard(
                    title = "tool call · ${item.toolName.ifBlank { item.toolCallId }}",
                    body = item.arguments.ifBlank { "{}" },
                    isResult = false,
                )
                is MessageItem.ToolResult -> ToolCard(
                    title = "tool result · ${item.toolName.ifBlank { item.toolCallId }}" +
                        item.fileAttachmentIndex?.let { " · file #$it" }.orEmpty(),
                    body = item.context?.takeIf { it.isNotBlank() } ?: "[no textual result]",
                    isResult = true,
                )
                else -> Unit
            }
        }
    }
}

@Composable
private fun ToolCard(
    title: String,
    body: String,
    isResult: Boolean,
) {
    var expanded by remember(title, body) { mutableStateOf(false) }
    Surface(
        modifier = Modifier
            .fillMaxWidth()
            .clickable { expanded = !expanded },
        shape = RoundedCornerShape(14.dp),
        color = FrostedSurface,
        border = BorderStroke(1.dp, FrostedBorder),
    ) {
        Column {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 12.dp, vertical = 10.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Row(
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Icon(
                        if (isResult) Icons.Filled.Code else Icons.Filled.Terminal,
                        contentDescription = null,
                        tint = CodeHeaderText,
                    )
                    Text(
                        text = title,
                        style = MaterialTheme.typography.labelLarge,
                        fontWeight = FontWeight.Bold,
                        color = CodeHeaderText,
                    )
                }
                Icon(
                    if (expanded) Icons.Filled.ExpandLess else Icons.Filled.ExpandMore,
                    contentDescription = if (expanded) "Hide" else "Show",
                    tint = CodeHeaderText,
                )
            }
            if (expanded) {
                SelectionContainer {
                    Text(
                        text = body.take(8_000),
                        modifier = Modifier
                            .fillMaxWidth()
                            .background(Color(0xFFF0F0F4))
                            .padding(12.dp),
                        style = MaterialTheme.typography.bodySmall,
                        fontFamily = FontFamily.Monospace,
                    )
                }
            }
        }
    }
}

@Composable
private fun AttachmentList(
    attachments: List<MessageAttachment>,
    previews: Map<String, AttachmentPreviewUiState>,
    onPreviewAttachment: (MessageAttachment) -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        attachments.forEach { attachment ->
            AttachmentCard(
                attachment = attachment,
                preview = previews[attachment.previewKey()],
                onPreviewAttachment = onPreviewAttachment,
            )
        }
    }
}

@Composable
private fun AttachmentCard(
    attachment: MessageAttachment,
    preview: AttachmentPreviewUiState?,
    onPreviewAttachment: (MessageAttachment) -> Unit,
) {
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(enabled = attachment.url.isNotBlank()) { onPreviewAttachment(attachment) },
    ) {
        Column(
            modifier = Modifier.padding(10.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Text(
                text = "${attachment.kind.ifBlank { "file" }} · ${attachment.name.ifBlank { "attachment-${attachment.index}" }}",
                style = MaterialTheme.typography.labelMedium,
                fontWeight = FontWeight.SemiBold,
            )
            Text(
                text = listOfNotNull(
                    attachment.mediaType,
                    attachment.sizeBytes?.let(::formatBytes),
                ).joinToString(" · ").ifBlank { "Tap to preview" },
                style = MaterialTheme.typography.bodySmall,
            )
            AttachmentPreview(preview)
            if (attachment.url.isNotBlank()) {
                Text(
                    text = "tap to load · ${attachment.url}",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
            }
        }
    }
}

@Composable
private fun AttachmentPreview(preview: AttachmentPreviewUiState?) {
    when {
        preview == null -> Unit
        preview.isLoading -> Row(
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            CircularProgressIndicator(modifier = Modifier.widthIn(max = 20.dp))
            Text("Loading preview...", style = MaterialTheme.typography.bodySmall)
        }
        preview.error != null -> Text(
            text = preview.error,
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.error,
        )
        preview.image != null -> Image(
            bitmap = preview.image.asImageBitmap(),
            contentDescription = "attachment preview",
            modifier = Modifier
                .fillMaxWidth()
                .widthIn(max = 420.dp),
            contentScale = ContentScale.FillWidth,
        )
        preview.text != null -> Text(
            text = preview.text,
            style = MaterialTheme.typography.bodySmall,
            fontFamily = FontFamily.Monospace,
            modifier = Modifier
                .fillMaxWidth()
                .background(MaterialTheme.colorScheme.surfaceVariant)
                .padding(8.dp),
        )
        preview.detail != null -> Text(
            text = preview.detail,
            style = MaterialTheme.typography.bodySmall,
        )
    }
}

private fun MessageAttachment.previewKey(): String = url.ifBlank { "$index:$name" }

private sealed interface MarkdownBlock {
    data class Text(val text: String) : MarkdownBlock
    data class Code(val language: String, val code: String) : MarkdownBlock
}

private fun markdownBlocks(text: String): List<MarkdownBlock> {
    if (text.isBlank()) return listOf(MarkdownBlock.Text(""))
    val blocks = mutableListOf<MarkdownBlock>()
    val pendingText = StringBuilder()
    val pendingCode = StringBuilder()
    var inCode = false
    var language = ""
    text.lines().forEach { line ->
        if (line.startsWith("```")) {
            if (inCode) {
                blocks += MarkdownBlock.Code(language, pendingCode.toString().trimEnd())
                pendingCode.clear()
                language = ""
                inCode = false
            } else {
                if (pendingText.isNotEmpty()) {
                    blocks += MarkdownBlock.Text(pendingText.toString().trimEnd())
                    pendingText.clear()
                }
                language = line.removePrefix("```").trim()
                inCode = true
            }
        } else if (inCode) {
            pendingCode.appendLine(line)
        } else {
            pendingText.appendLine(line)
        }
    }
    if (inCode) {
        blocks += MarkdownBlock.Code(language, pendingCode.toString().trimEnd())
    }
    if (pendingText.isNotEmpty()) {
        blocks += MarkdownBlock.Text(pendingText.toString().trimEnd())
    }
    return blocks.ifEmpty { listOf(MarkdownBlock.Text(text)) }
}

private fun formatBytes(value: Long): String {
    val units = listOf("B", "KB", "MB", "GB")
    var size = value.toDouble()
    var unit = 0
    while (size >= 1024 && unit < units.lastIndex) {
        size /= 1024
        unit += 1
    }
    return if (unit == 0) {
        "${value}B"
    } else {
        "${String.format("%.1f", size)}${units[unit]}"
    }
}

private fun formatCompactNumber(value: Long): String = when {
    value >= 1_000_000 -> "${String.format("%.1f", value / 1_000_000.0)}M"
    value >= 1_000 -> {
        val rounded = value / 1_000.0
        if (rounded >= 100) "${(rounded).toInt()}K" else "${String.format("%.1f", rounded)}K"
    }
    else -> value.toString()
}

private fun formatLocalMinute(value: String): String = runCatching {
    Instant.parse(value)
        .atZone(ZoneId.systemDefault())
        .format(LocalMinuteFormatter)
}.getOrElse { value.take(16) }

private val LocalMinuteFormatter: DateTimeFormatter = DateTimeFormatter.ofPattern("yyyy-MM-dd HH:mm")

@Composable
private fun Composer(
    draft: String,
    pendingAttachments: List<PendingAttachmentUiState>,
    isSending: Boolean,
    onDraftChanged: (String) -> Unit,
    onAddAttachments: (List<android.net.Uri>) -> Unit,
    onRemoveAttachment: (String) -> Unit,
    onSend: () -> Unit,
) {
    val attachmentLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.GetMultipleContents(),
    ) { uris -> onAddAttachments(uris) }
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        if (pendingAttachments.isNotEmpty()) {
            Surface(
                shape = RoundedCornerShape(18.dp),
                color = FrostedSurface,
                border = BorderStroke(1.dp, FrostedBorder),
            ) {
            Column(modifier = Modifier.padding(10.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                pendingAttachments.forEach { attachment ->
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.SpaceBetween,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Column(modifier = Modifier.weight(1f)) {
                            Text(text = attachment.name, style = MaterialTheme.typography.labelMedium)
                            Text(
                                text = listOfNotNull(
                                    attachment.mediaType,
                                    attachment.sizeBytes?.let(::formatBytes),
                                ).joinToString(" · ").ifBlank { "attachment" },
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                        IconButton(onClick = { onRemoveAttachment(attachment.uri) }) {
                            Icon(Icons.Filled.Close, contentDescription = "Remove attachment")
                        }
                    }
                }
            }
            }
        }
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(10.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Surface(
                modifier = Modifier.size(58.dp),
                shape = CircleShape,
                color = FrostedSurface,
                border = BorderStroke(1.dp, FrostedBorder),
            ) {
                IconButton(
                    onClick = { attachmentLauncher.launch("*/*") },
                    enabled = !isSending,
                ) {
                    Icon(Icons.Filled.AttachFile, contentDescription = "Attach files", tint = Color(0xFF111111))
                }
            }
            OutlinedTextField(
                value = draft,
                onValueChange = onDraftChanged,
                modifier = Modifier.weight(1f),
                placeholder = { Text("消息", color = MutedText) },
                minLines = 1,
                maxLines = 4,
                shape = RoundedCornerShape(28.dp),
                colors = OutlinedTextFieldDefaults.colors(
                    focusedContainerColor = FrostedSurface,
                    unfocusedContainerColor = FrostedSurface,
                    disabledContainerColor = FrostedSurface,
                    focusedBorderColor = FrostedBorder,
                    unfocusedBorderColor = FrostedBorder,
                ),
            )
            Surface(
                modifier = Modifier.size(58.dp),
                shape = CircleShape,
                color = if ((draft.isNotBlank() || pendingAttachments.isNotEmpty()) && !isSending) {
                    Color(0xFF7A7A7A)
                } else {
                    Color(0x33808080)
                },
            ) {
                IconButton(
                    onClick = onSend,
                    enabled = (draft.isNotBlank() || pendingAttachments.isNotEmpty()) && !isSending,
                ) {
                    Icon(
                        Icons.AutoMirrored.Filled.Send,
                        contentDescription = if (isSending) "Sending" else "Send",
                        tint = Color.White,
                    )
                }
            }
        }
        Spacer(modifier = Modifier.height(6.dp))
    }
}

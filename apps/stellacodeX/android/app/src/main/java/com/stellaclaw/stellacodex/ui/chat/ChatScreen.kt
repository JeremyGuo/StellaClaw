package com.stellaclaw.stellacodex.ui.chat

import android.app.Application
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import com.stellaclaw.stellacodex.domain.model.ChatMessage
import com.stellaclaw.stellacodex.domain.model.MessageAttachment
import com.stellaclaw.stellacodex.domain.model.MessageItem
import com.stellaclaw.stellacodex.domain.model.MessageLocalState

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

    LaunchedEffect(conversationId) {
        viewModel.load(conversationId)
    }

    LaunchedEffect(state.messages.size, state.messages.lastOrNull()?.id) {
        if (state.messages.isNotEmpty()) {
            listState.animateScrollToItem(state.messages.lastIndex)
        }
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(conversationId.ifBlank { "Conversation" }) },
                navigationIcon = { TextButton(onClick = onBack) { Text("Back") } },
                actions = {
                    TextButton(onClick = viewModel::refresh) { Text("Refresh") }
                    TextButton(onClick = { onOpenWorkspace(conversationId) }) { Text("Files") }
                },
            )
        },
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            state.error?.let { message ->
                Text(
                    text = message,
                    color = MaterialTheme.colorScheme.error,
                    style = MaterialTheme.typography.bodyMedium,
                )
            }
            HistoryStatus(
                loadedOffset = state.loadedOffset,
                loadedCount = state.messages.size,
                totalMessages = state.totalMessages,
            )

            when {
                state.isLoading && state.messages.isEmpty() -> LoadingMessages()
                state.messages.isEmpty() -> EmptyMessages()
                else -> MessageList(
                    messages = state.messages,
                    listState = listState,
                    previews = state.attachmentPreviews,
                    onPreviewAttachment = viewModel::previewAttachment,
                    modifier = Modifier.weight(1f),
                )
            }

            Composer(
                draft = state.draft,
                isSending = state.isSending,
                onDraftChanged = viewModel::onDraftChanged,
                onSend = viewModel::send,
            )
        }
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
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(
            modifier = Modifier.padding(10.dp),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            if (realtimeState.isNotBlank()) {
                Text(
                    text = realtimeState,
                    style = MaterialTheme.typography.labelMedium,
                    color = if (realtimeState.contains("error", ignoreCase = true) ||
                        realtimeState.contains("unavailable", ignoreCase = true)
                    ) {
                        MaterialTheme.colorScheme.error
                    } else {
                        MaterialTheme.colorScheme.primary
                    },
                )
            }
            progressTitle?.let { title ->
                Text(
                    text = if (progressImportant) "! $title" else title,
                    style = MaterialTheme.typography.bodyMedium,
                )
            }
            progressDetail?.let { detail ->
                Text(text = detail, style = MaterialTheme.typography.bodySmall)
            }
        }
    }
}

@Composable
private fun HistoryStatus(
    loadedOffset: Int,
    loadedCount: Int,
    totalMessages: Int,
) {
    if (totalMessages <= 0 || loadedCount <= 0) return
    val start = loadedOffset + 1
    val end = loadedOffset + loadedCount
    Text(
        text = "Showing latest messages $start-$end of $totalMessages",
        style = MaterialTheme.typography.labelSmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
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
    messages: List<ChatMessage>,
    listState: LazyListState,
    previews: Map<String, AttachmentPreviewUiState>,
    onPreviewAttachment: (MessageAttachment) -> Unit,
    modifier: Modifier = Modifier,
) {
    LazyColumn(
        modifier = modifier.fillMaxWidth(),
        state = listState,
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        items(messages, key = { it.id }) { message ->
            MessageCard(
                message = message,
                previews = previews,
                onPreviewAttachment = onPreviewAttachment,
            )
        }
    }
}

@Composable
private fun MessageCard(
    message: ChatMessage,
    previews: Map<String, AttachmentPreviewUiState>,
    onPreviewAttachment: (MessageAttachment) -> Unit,
) {
    val roleLabel = when (message.role.lowercase()) {
        "user" -> message.userName ?: "You"
        "assistant" -> "Assistant"
        "system" -> "System"
        else -> message.role.ifBlank { "Message" }
    }
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(
            modifier = Modifier.padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
            ) {
                Text(text = roleLabel, style = MaterialTheme.typography.labelMedium)
                Text(text = "#${message.index}", style = MaterialTheme.typography.labelSmall)
            }
            MessageBody(text = message.text.ifBlank { message.preview.ifBlank { "[empty message]" } })
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
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                message.messageTime?.let { time ->
                    Text(text = time, style = MaterialTheme.typography.labelSmall)
                }
                when (message.localState) {
                    MessageLocalState.Sending -> Text(
                        text = "sending...",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.primary,
                    )
                    MessageLocalState.Failed -> Text(
                        text = "send failed",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.error,
                    )
                    MessageLocalState.Synced -> Unit
                }
                if (message.attachmentCount > 0) {
                    Text(
                        text = "${message.attachmentCount} attachments",
                        style = MaterialTheme.typography.labelSmall,
                    )
                }
                if (message.hasAttachmentErrors) {
                    Text(
                        text = "attachment errors",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.error,
                    )
                }
                if (message.tokenUsage != null) {
                    val usage = message.tokenUsage
                    Text(
                        text = "tokens ${usage.total} (${usage.input} in / ${usage.output} out)",
                        style = MaterialTheme.typography.labelSmall,
                    )
                } else if (message.hasTokenUsage) {
                    Text(text = "usage", style = MaterialTheme.typography.labelSmall)
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
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(MaterialTheme.colorScheme.surfaceVariant)
            .padding(10.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        if (block.language.isNotBlank()) {
            Text(
                text = block.language,
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.primary,
            )
        }
        Text(
            text = block.code.ifBlank { " " },
            style = MaterialTheme.typography.bodySmall,
            fontFamily = FontFamily.Monospace,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
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
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(if (isResult) MaterialTheme.colorScheme.secondaryContainer else MaterialTheme.colorScheme.tertiaryContainer)
            .padding(10.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        Text(
            text = title,
            style = MaterialTheme.typography.labelMedium,
            fontWeight = FontWeight.SemiBold,
        )
        Text(
            text = body.take(8_000),
            style = MaterialTheme.typography.bodySmall,
            fontFamily = FontFamily.Monospace,
        )
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

@Composable
private fun Composer(
    draft: String,
    isSending: Boolean,
    onDraftChanged: (String) -> Unit,
    onSend: () -> Unit,
) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.spacedBy(8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        OutlinedTextField(
            value = draft,
            onValueChange = onDraftChanged,
            modifier = Modifier.weight(1f),
            placeholder = { Text("Message Stellaclaw") },
            minLines = 1,
            maxLines = 4,
        )
        Button(
            onClick = onSend,
            enabled = draft.isNotBlank() && !isSending,
        ) {
            Text(if (isSending) "Sending" else "Send")
        }
    }
}

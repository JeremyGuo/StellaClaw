package com.stellaclaw.stellacodex.domain.model

data class ChatMessage(
    val id: String,
    val index: Int,
    val role: String,
    val text: String,
    val preview: String,
    val userName: String?,
    val messageTime: String?,
    val attachmentCount: Int,
    val attachments: List<MessageAttachment> = emptyList(),
    val items: List<MessageItem> = emptyList(),
    val hasAttachmentErrors: Boolean = false,
    val hasTokenUsage: Boolean,
    val tokenUsage: MessageTokenUsage? = null,
    val localState: MessageLocalState = MessageLocalState.Synced,
)

data class MessageAttachment(
    val index: Int,
    val kind: String,
    val name: String,
    val mediaType: String?,
    val sizeBytes: Long?,
    val url: String,
)

sealed interface MessageItem {
    val index: Int

    data class Text(
        override val index: Int,
        val text: String,
    ) : MessageItem

    data class File(
        override val index: Int,
        val attachmentIndex: Int,
    ) : MessageItem

    data class ToolCall(
        override val index: Int,
        val toolCallId: String,
        val toolName: String,
        val arguments: String,
    ) : MessageItem

    data class ToolResult(
        override val index: Int,
        val toolCallId: String,
        val toolName: String,
        val context: String?,
        val fileAttachmentIndex: Int?,
    ) : MessageItem
}

data class MessageTokenUsage(
    val input: Long,
    val output: Long,
    val total: Long,
)

enum class MessageLocalState {
    Synced,
    Sending,
    Failed,
}

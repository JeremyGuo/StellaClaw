package com.stellaclaw.stellacodex.data.dto

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonObject

@Serializable
data class MessagesResponseDto(
    @SerialName("conversation_id") val conversationId: String = "",
    val offset: Int = 0,
    val limit: Int = 0,
    val total: Int = 0,
    val messages: List<ChatMessageDto> = emptyList(),
)

@Serializable
data class ChatMessageDto(
    val id: String = "",
    val index: Int = 0,
    val role: String = "",
    val text: String = "",
    val preview: String = "",
    val items: List<JsonObject> = emptyList(),
    val attachments: List<MessageAttachmentDto> = emptyList(),
    @SerialName("has_attachment_errors") val hasAttachmentErrors: Boolean = false,
    @SerialName("user_name") val userName: String? = null,
    @SerialName("message_time") val messageTime: String? = null,
    @SerialName("attachment_count") val attachmentCount: Int = 0,
    @SerialName("has_token_usage") val hasTokenUsage: Boolean = false,
    @SerialName("token_usage") val tokenUsage: MessageTokenUsageDto? = null,
)

@Serializable
data class MessageAttachmentDto(
    val index: Int = 0,
    val kind: String = "document",
    val name: String = "",
    @SerialName("media_type") val mediaType: String? = null,
    @SerialName("size_bytes") val sizeBytes: Long? = null,
    val url: String = "",
)

@Serializable
data class MessageTokenUsageDto(
    val input: Long = 0,
    val output: Long = 0,
    val total: Long = 0,
)

@Serializable
data class MarkConversationSeenRequestDto(
    @SerialName("last_seen_message_id") val lastSeenMessageId: String,
)

@Serializable
data class SendMessageRequestDto(
    @SerialName("user_name") val userName: String,
    @SerialName("message_time") val messageTime: String? = null,
    val text: String,
    val files: List<SendMessageFileDto> = emptyList(),
    @SerialName("remote_message_id") val remoteMessageId: String? = null,
)

@Serializable
data class SendMessageFileDto(
    val uri: String,
    @SerialName("media_type") val mediaType: String? = null,
    val name: String? = null,
)

@Serializable
data class SendMessageResponseDto(
    @SerialName("conversation_id") val conversationId: String = "",
    @SerialName("remote_message_id") val remoteMessageId: String = "",
    val accepted: Boolean = false,
)

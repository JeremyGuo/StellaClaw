package com.stellaclaw.stellacodex.data.dto

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

@Serializable
data class ConversationsResponseDto(
    @SerialName("channel_id") val channelId: String = "",
    val offset: Int = 0,
    val limit: Int = 0,
    val total: Int = 0,
    val conversations: List<ConversationSummaryDto> = emptyList(),
)

@Serializable
data class ConversationSummaryDto(
    @SerialName("conversation_id") val conversationId: String = "",
    @SerialName("platform_chat_id") val platformChatId: String = "",
    val nickname: String? = null,
    val model: String = "",
    @SerialName("model_selection_pending") val modelSelectionPending: Boolean = false,
    val reasoning: String = "",
    val sandbox: String = "",
    @SerialName("sandbox_source") val sandboxSource: String = "",
    val remote: String = "",
    val workspace: String = "",
    @SerialName("foreground_session_id") val foregroundSessionId: String = "",
    @SerialName("total_background") val totalBackground: Int = 0,
    @SerialName("total_subagents") val totalSubagents: Int = 0,
    @SerialName("processing_state") val processingState: String = "idle",
    val running: Boolean = false,
    @SerialName("message_count") val messageCount: Int = 0,
    @SerialName("last_message_id") val lastMessageId: String? = null,
    @SerialName("last_message_time") val lastMessageTime: String? = null,
    @SerialName("last_seen_message_id") val lastSeenMessageId: String? = null,
    @SerialName("last_seen_at") val lastSeenAt: String? = null,
)

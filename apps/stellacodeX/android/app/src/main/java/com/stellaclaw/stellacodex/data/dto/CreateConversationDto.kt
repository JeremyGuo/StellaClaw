package com.stellaclaw.stellacodex.data.dto

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

@Serializable
data class CreateConversationRequestDto(
    @SerialName("platform_chat_id") val platformChatId: String? = null,
    val model: String? = null,
    val nickname: String? = null,
)

@Serializable
data class CreateConversationResponseDto(
    @SerialName("conversation_id") val conversationId: String = "",
    val nickname: String = "",
    @SerialName("channel_id") val channelId: String = "",
    @SerialName("platform_chat_id") val platformChatId: String = "",
    @SerialName("model_selection_pending") val modelSelectionPending: Boolean = false,
)

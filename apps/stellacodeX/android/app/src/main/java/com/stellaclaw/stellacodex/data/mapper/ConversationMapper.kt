package com.stellaclaw.stellacodex.data.mapper

import com.stellaclaw.stellacodex.data.dto.ConversationSummaryDto
import com.stellaclaw.stellacodex.domain.model.ConversationSummary

fun ConversationSummaryDto.toDomain(): ConversationSummary = ConversationSummary(
    conversationId = conversationId,
    platformChatId = platformChatId,
    displayName = nickname?.trim().takeUnless { it.isNullOrBlank() }
        ?: platformChatId.takeIf { it.isNotBlank() }
        ?: conversationId,
    model = model,
    modelSelectionPending = modelSelectionPending,
    reasoning = reasoning,
    sandbox = sandbox,
    sandboxSource = sandboxSource,
    remote = remote,
    workspace = workspace,
    foregroundSessionId = foregroundSessionId,
    totalBackground = totalBackground,
    totalSubagents = totalSubagents,
    processingState = processingState,
    running = running,
    messageCount = messageCount,
    lastMessageId = lastMessageId,
    lastMessageTime = lastMessageTime,
    lastSeenMessageId = lastSeenMessageId,
    lastSeenAt = lastSeenAt,
)

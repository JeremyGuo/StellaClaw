package com.stellaclaw.stellacodex.data.mapper

import com.stellaclaw.stellacodex.data.dto.ChatMessageDto
import com.stellaclaw.stellacodex.data.dto.MessageAttachmentDto
import com.stellaclaw.stellacodex.data.dto.MessageTokenUsageDto
import com.stellaclaw.stellacodex.domain.model.ChatMessage
import com.stellaclaw.stellacodex.domain.model.MessageAttachment
import com.stellaclaw.stellacodex.domain.model.MessageItem
import com.stellaclaw.stellacodex.domain.model.MessageTokenUsage
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonPrimitive

fun ChatMessageDto.toDomain(): ChatMessage = ChatMessage(
    id = id,
    index = index,
    role = role,
    text = text,
    preview = preview,
    userName = userName,
    messageTime = messageTime,
    attachmentCount = attachmentCount,
    attachments = attachments.map { it.toDomain() },
    items = items.mapNotNull { it.toMessageItem() },
    hasAttachmentErrors = hasAttachmentErrors,
    hasTokenUsage = hasTokenUsage,
    tokenUsage = tokenUsage?.toDomain(),
)

private fun MessageAttachmentDto.toDomain(): MessageAttachment = MessageAttachment(
    index = index,
    kind = kind,
    name = name,
    mediaType = mediaType,
    sizeBytes = sizeBytes,
    url = url,
)

private fun MessageTokenUsageDto.toDomain(): MessageTokenUsage = MessageTokenUsage(
    input = input,
    output = output,
    total = total,
)

private fun JsonObject.toMessageItem(): MessageItem? {
    val type = string("type")
    val index = int("index") ?: 0
    return when (type) {
        "text" -> MessageItem.Text(
            index = index,
            text = string("text").orEmpty(),
        )
        "file" -> MessageItem.File(
            index = index,
            attachmentIndex = int("attachment_index") ?: -1,
        )
        "tool_call" -> MessageItem.ToolCall(
            index = index,
            toolCallId = string("tool_call_id").orEmpty(),
            toolName = string("tool_name").orEmpty(),
            arguments = get("arguments")?.compactJson().orEmpty(),
            explanation = string("explanation") ?: (get("arguments") as? JsonObject)?.string("explanation"),
        )
        "tool_result" -> MessageItem.ToolResult(
            index = index,
            toolCallId = string("tool_call_id").orEmpty(),
            toolName = string("tool_name").orEmpty(),
            context = string("context"),
            fileAttachmentIndex = int("file_attachment_index"),
        )
        else -> null
    }
}

private fun JsonObject.string(name: String): String? = get(name)?.let { value ->
    if (value is JsonPrimitive && value.isString) value.content else null
}

private fun JsonObject.int(name: String): Int? = get(name)?.jsonPrimitive?.intOrNull

private fun JsonElement.compactJson(): String = when (this) {
    JsonNull -> "null"
    is JsonPrimitive -> content
    else -> toString()
}

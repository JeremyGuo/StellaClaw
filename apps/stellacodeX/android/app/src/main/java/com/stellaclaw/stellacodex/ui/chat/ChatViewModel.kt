package com.stellaclaw.stellacodex.ui.chat

import android.app.Application
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.stellaclaw.stellacodex.core.result.AppResult
import com.stellaclaw.stellacodex.core.result.userMessage
import com.stellaclaw.stellacodex.data.api.StellaclawApi
import com.stellaclaw.stellacodex.data.dto.MessagesResponseDto
import com.stellaclaw.stellacodex.data.mapper.toDomain
import com.stellaclaw.stellacodex.data.store.ConnectionProfileStore
import com.stellaclaw.stellacodex.data.store.connectionDataStore
import com.stellaclaw.stellacodex.domain.model.ChatMessage
import com.stellaclaw.stellacodex.domain.model.ConnectionProfile
import com.stellaclaw.stellacodex.domain.model.MessageAttachment
import com.stellaclaw.stellacodex.domain.model.MessageLocalState
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.serialization.SerializationException
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonPrimitive
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import java.time.Instant
import kotlin.math.min
import kotlin.text.Charsets

class ChatViewModel(application: Application) : AndroidViewModel(application) {
    private val store = ConnectionProfileStore(application.connectionDataStore)
    private val api = StellaclawApi()
    private val json = Json {
        ignoreUnknownKeys = true
        explicitNulls = false
    }

    private val mutableState = MutableStateFlow(ChatUiState())
    val state: StateFlow<ChatUiState> = mutableState.asStateFlow()
    private var webSocket: WebSocket? = null
    private var realtimeConversationId: String = ""
    private var reconnectJob: Job? = null
    private var reconnectAttempt: Int = 0
    private var reconnectEnabled: Boolean = false
    private var latestProfile: ConnectionProfile? = null

    fun load(conversationId: String) {
        if (conversationId.isBlank()) return
        if (state.value.conversationId == conversationId && webSocket != null) return
        closeRealtime(allowReconnect = false)
        mutableState.update {
            it.copy(
                conversationId = conversationId,
                messages = emptyList(),
                realtimeState = "Connecting realtime...",
                progressTitle = null,
                progressDetail = null,
            )
        }
        refresh(connectRealtimeAfterLoad = true)
    }

    fun onDraftChanged(value: String) {
        mutableState.update { it.copy(draft = value, error = null) }
    }

    fun previewAttachment(attachment: MessageAttachment) {
        val key = attachment.previewKey()
        val current = state.value.attachmentPreviews[key]
        if (current?.isLoading == true || current?.hasContent == true) return
        viewModelScope.launch {
            mutableState.update {
                it.copy(
                    attachmentPreviews = it.attachmentPreviews + (key to AttachmentPreviewUiState(isLoading = true)),
                )
            }
            val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
            when (val result = api.fetchAttachment(profile, attachment.url)) {
                is AppResult.Ok -> {
                    val mediaType = attachment.mediaType ?: result.value.mediaType.orEmpty()
                    val image = if (attachment.kind == "image" || mediaType.startsWith("image/")) {
                        BitmapFactory.decodeByteArray(result.value.bytes, 0, result.value.bytes.size)
                    } else {
                        null
                    }
                    val text = if (image == null && isTextAttachment(mediaType, attachment.name, result.value.bytes.size)) {
                        result.value.bytes.toString(Charsets.UTF_8).take(16_000)
                    } else {
                        null
                    }
                    mutableState.update {
                        it.copy(
                            attachmentPreviews = it.attachmentPreviews + (key to AttachmentPreviewUiState(
                                isLoading = false,
                                image = image,
                                text = text,
                                detail = if (image == null && text == null) "Loaded ${formatBytes(result.value.bytes.size.toLong())}" else null,
                            )),
                        )
                    }
                }
                is AppResult.Err -> mutableState.update {
                    it.copy(
                        attachmentPreviews = it.attachmentPreviews + (key to AttachmentPreviewUiState(
                            isLoading = false,
                            error = result.error.userMessage(),
                        )),
                    )
                }
            }
        }
    }

    fun refresh(connectRealtimeAfterLoad: Boolean = false, showLoading: Boolean = true) {
        val conversationId = state.value.conversationId
        if (conversationId.isBlank()) return
        viewModelScope.launch {
            if (showLoading) {
                mutableState.update { it.copy(isLoading = true, error = null) }
            } else {
                mutableState.update { it.copy(error = null) }
            }
            val profile = store.profile.first()
            latestProfile = profile
            when (val result = api.loadLatestMessages(profile, conversationId)) {
                is AppResult.Ok -> {
                    mutableState.update {
                        it.copy(
                            isLoading = false,
                            messages = trimToLatestMessages(mergeMessages(it.messages, result.value.messages)),
                            loadedOffset = result.value.offset,
                            totalMessages = result.value.total,
                            error = null,
                        )
                    }
                    if (connectRealtimeAfterLoad) {
                        connectRealtime(profile, conversationId)
                    }
                }
                is AppResult.Err -> {
                    mutableState.update {
                        it.copy(
                            isLoading = false,
                            error = result.error.userMessage(),
                            realtimeState = "Realtime unavailable; use Refresh",
                        )
                    }
                    if (connectRealtimeAfterLoad) {
                        connectRealtime(profile, conversationId)
                    }
                }
            }
        }
    }

    fun send() {
        val current = state.value
        val text = current.draft.trim()
        if (current.conversationId.isBlank() || text.isEmpty() || current.isSending) return
        val localId = "local-${System.currentTimeMillis()}"
        val optimistic = ChatMessage(
            id = localId,
            index = nextLocalIndex(current.messages),
            role = "user",
            text = text,
            preview = text,
            userName = "Stellacode",
            messageTime = Instant.now().toString(),
            attachmentCount = 0,
            attachments = emptyList(),
            items = emptyList(),
            hasAttachmentErrors = false,
            hasTokenUsage = false,
            localState = MessageLocalState.Sending,
        )
        mutableState.update {
            it.copy(
                draft = "",
                isSending = true,
                error = null,
                messages = it.messages + optimistic,
            )
        }
        viewModelScope.launch {
            val profile: ConnectionProfile = store.profile.first()
            latestProfile = profile
            when (val result = api.sendMessage(profile, current.conversationId, text)) {
                is AppResult.Ok -> {
                    mutableState.update { state ->
                        state.copy(
                            isSending = false,
                            messages = state.messages.map { message ->
                                if (message.id == localId) {
                                    message.copy(localState = MessageLocalState.Sending)
                                } else {
                                    message
                                }
                            },
                        )
                    }
                    delay(350)
                    refresh()
                }
                is AppResult.Err -> mutableState.update { state ->
                    state.copy(
                        isSending = false,
                        error = result.error.userMessage(),
                        messages = state.messages.map { message ->
                            if (message.id == localId) {
                                message.copy(localState = MessageLocalState.Failed)
                            } else {
                                message
                            }
                        },
                    )
                }
            }
        }
    }

    private suspend fun connectRealtime(profile: ConnectionProfile, conversationId: String) {
        reconnectEnabled = true
        latestProfile = profile
        reconnectJob?.cancel()
        when (val result = api.foregroundWebSocketRequest(profile, conversationId)) {
            is AppResult.Ok -> {
                realtimeConversationId = conversationId
                webSocket?.cancel()
                webSocket = api.client.newWebSocket(result.value, ChatWebSocketListener(conversationId))
            }
            is AppResult.Err -> {
                mutableState.update { it.copy(realtimeState = result.error.userMessage()) }
                scheduleReconnect(conversationId)
            }
        }
    }

    private fun scheduleReconnect(conversationId: String) {
        if (!reconnectEnabled || conversationId != realtimeConversationId && realtimeConversationId.isNotBlank()) return
        reconnectJob?.cancel()
        val delayMillis = min(30_000L, 1_000L * (1 shl min(reconnectAttempt, 5)))
        reconnectAttempt += 1
        mutableState.update { it.copy(realtimeState = "Realtime reconnecting in ${delayMillis / 1000}s") }
        reconnectJob = viewModelScope.launch {
            delay(delayMillis)
            val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
            if (state.value.conversationId == conversationId && reconnectEnabled) {
                mutableState.update { it.copy(realtimeState = "Reconnecting realtime...") }
                connectRealtime(profile, conversationId)
            }
        }
    }

    private fun closeRealtime(allowReconnect: Boolean) {
        reconnectEnabled = allowReconnect
        reconnectJob?.cancel()
        reconnectJob = null
        webSocket?.close(1000, "conversation changed")
        webSocket = null
        if (!allowReconnect) {
            realtimeConversationId = ""
            reconnectAttempt = 0
        }
    }

    private fun applyIncomingMessages(incoming: List<ChatMessage>) {
        if (incoming.isEmpty()) return
        mutableState.update { current ->
            current.copy(messages = trimToLatestMessages(mergeMessages(current.messages, incoming)))
        }
    }

    private fun mergeMessages(existing: List<ChatMessage>, incoming: List<ChatMessage>): List<ChatMessage> {
        val syncedIncoming = incoming.map { it.copy(localState = MessageLocalState.Synced) }
        val byId = linkedMapOf<String, ChatMessage>()
        existing.filterNot { local ->
            local.localState == MessageLocalState.Sending && syncedIncoming.any { remote ->
                remote.role == local.role && remote.text == local.text && remote.userName == local.userName
            }
        }.forEach { byId[it.id] = it }
        syncedIncoming.forEach { byId[it.id] = it }
        return byId.values.sortedWith(compareBy<ChatMessage> { it.index }.thenBy { it.id })
    }

    private fun trimToLatestMessages(messages: List<ChatMessage>, maxSynced: Int = LatestMessageLimit): List<ChatMessage> {
        val synced = messages
            .filter { it.localState == MessageLocalState.Synced }
            .sortedWith(compareBy<ChatMessage> { it.index }.thenBy { it.id })
            .takeLast(maxSynced)
        val local = messages.filter { it.localState != MessageLocalState.Synced }
        return (synced + local).sortedWith(compareBy<ChatMessage> { it.index }.thenBy { it.id })
    }

    private fun lastServerMessageIndex(messages: List<ChatMessage>): Int? = messages
        .asReversed()
        .firstOrNull { it.localState == MessageLocalState.Synced && it.id.toIntOrNull() != null }
        ?.id
        ?.toIntOrNull()

    private fun handleSubscriptionAck(conversationId: String, payload: JsonObject) {
        val currentMessageId = (payload["current_message_id"] as? JsonPrimitive)?.content?.toIntOrNull()
        val nextMessageId = (payload["next_message_id"] as? JsonPrimitive)?.content?.toIntOrNull()
            ?: (payload["total"] as? JsonPrimitive)?.intOrNull
            ?: return
        viewModelScope.launch {
            if (state.value.conversationId != conversationId || conversationId != realtimeConversationId) return@launch
            val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
            val lastLocalId = lastServerMessageIndex(state.value.messages)
            val request = when {
                lastLocalId == null -> {
                    val latestIndex = currentMessageId ?: nextMessageId.minus(1)
                    if (latestIndex < 0) null else {
                        val offset = (latestIndex - LatestMessageLimit + 1).coerceAtLeast(0)
                        offset to LatestMessageLimit
                    }
                }
                nextMessageId > lastLocalId + 1 -> {
                    val gap = (nextMessageId - lastLocalId - 1).coerceIn(1, 200)
                    (lastLocalId + 1) to gap
                }
                else -> null
            } ?: return@launch
            when (val result = api.loadMessagePage(profile, conversationId, offset = request.first, limit = request.second)) {
                is AppResult.Ok -> {
                    mutableState.update {
                        it.copy(
                            messages = trimToLatestMessages(mergeMessages(it.messages, result.value.messages)),
                            loadedOffset = result.value.offset,
                            totalMessages = result.value.total,
                            error = null,
                            realtimeState = "Realtime synced",
                        )
                    }
                }
                is AppResult.Err -> mutableState.update {
                    it.copy(realtimeState = "Realtime sync gap failed: ${result.error.userMessage()}")
                }
            }
        }
    }

    private fun nextLocalIndex(messages: List<ChatMessage>): Int =
        (messages.maxOfOrNull { it.index } ?: -1) + 1

    private fun MessageAttachment.previewKey(): String = url.ifBlank { "$index:$name" }

    private fun isTextAttachment(mediaType: String, name: String, byteCount: Int): Boolean {
        if (byteCount > 256 * 1024) return false
        if (mediaType.startsWith("text/")) return true
        if (mediaType.contains("json") || mediaType.contains("xml")) return true
        val lower = name.lowercase()
        return listOf(".txt", ".md", ".json", ".xml", ".log", ".csv", ".kt", ".rs", ".js", ".ts", ".py", ".toml", ".yaml", ".yml")
            .any { lower.endsWith(it) }
    }

    private fun formatBytes(value: Long): String {
        val units = listOf("B", "KB", "MB", "GB")
        var size = value.toDouble()
        var unit = 0
        while (size >= 1024 && unit < units.lastIndex) {
            size /= 1024
            unit += 1
        }
        return if (unit == 0) "${value}B" else "${String.format("%.1f", size)}${units[unit]}"
    }

    private fun handleWebSocketText(conversationId: String, text: String) {
        if (conversationId != realtimeConversationId) return
        try {
            val payload = json.decodeFromString<JsonObject>(text)
            when (payload["type"]?.jsonPrimitive?.content) {
                "subscription_ack" -> {
                    val reason = payload["reason"]?.jsonPrimitive?.content.orEmpty()
                    reconnectAttempt = 0
                    mutableState.update {
                        it.copy(
                            realtimeState = if (reason == "session_changed") {
                                "Realtime synced; session changed"
                            } else {
                                "Realtime connected"
                            },
                        )
                    }
                    if (payload["turn_progress"] is JsonObject) {
                        handleProgress(payload["turn_progress"] as JsonObject)
                    }
                    handleSubscriptionAck(conversationId, payload)
                }
                "messages" -> {
                    val page = json.decodeFromString<MessagesResponseDto>(text)
                    val messages = page.messages.map { it.toDomain() }
                    applyIncomingMessages(messages)
                    reconnectAttempt = 0
                    mutableState.update {
                        it.copy(
                            loadedOffset = page.offset,
                            totalMessages = page.total,
                            realtimeState = "Realtime connected · ${page.offset + messages.size}/${page.total}",
                        )
                    }
                }
                "turn_progress" -> handleProgress(payload)
                "error" -> mutableState.update {
                    it.copy(
                        realtimeState = payload["message"]?.jsonPrimitive?.content
                            ?: payload["error"]?.jsonPrimitive?.content
                            ?: "Realtime error",
                    )
                }
            }
        } catch (_: SerializationException) {
            // Ignore malformed realtime frames; manual Refresh remains available.
        } catch (_: IllegalArgumentException) {
            // Ignore unexpected payload shapes.
        }
    }

    private fun handleProgress(payload: JsonObject) {
        val finalState = payload["final_state"]?.jsonPrimitive?.content
        val progress = payload["progress"] as? JsonObject
        val phase = payload["phase"]?.jsonPrimitive?.content
            ?: progress?.get("phase")?.jsonPrimitive?.content
        val activity = payload["activity"]?.jsonPrimitive?.content
            ?: progress?.get("activity")?.jsonPrimitive?.content
        val hint = payload["hint"]?.jsonPrimitive?.content
            ?: progress?.get("hint")?.jsonPrimitive?.content
        val important = payload["important"]?.jsonPrimitive?.booleanOrNull ?: false
        val title = when (finalState) {
            "done" -> "Done"
            "failed" -> "Failed"
            else -> phase?.replaceFirstChar { it.uppercase() } ?: "Working"
        }
        val detail = listOfNotNull(activity, hint).joinToString(" · ").ifBlank { null }
        mutableState.update {
            it.copy(
                progressTitle = title,
                progressDetail = detail,
                progressImportant = important,
                realtimeState = if (finalState == null) "Realtime active" else "Realtime connected",
            )
        }
        if (finalState == "done" || finalState == "failed") {
            viewModelScope.launch {
                delay(500)
                refresh()
                delay(1200)
                mutableState.update { it.copy(progressTitle = null, progressDetail = null, progressImportant = false) }
            }
        }
    }

    override fun onCleared() {
        closeRealtime(allowReconnect = false)
        super.onCleared()
    }

    private inner class ChatWebSocketListener(
        private val conversationId: String,
    ) : WebSocketListener() {
        override fun onOpen(webSocket: WebSocket, response: Response) {
            reconnectAttempt = 0
            mutableState.update { it.copy(realtimeState = "Realtime connected") }
        }

        override fun onMessage(webSocket: WebSocket, text: String) {
            handleWebSocketText(conversationId, text)
        }

        override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
            webSocket.close(code, reason)
        }

        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
            if (conversationId == realtimeConversationId && reconnectEnabled) {
                scheduleReconnect(conversationId)
            }
        }

        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
            if (conversationId == realtimeConversationId && reconnectEnabled) {
                mutableState.update {
                    it.copy(realtimeState = "Realtime error: ${t.message.orEmpty().ifBlank { "unknown" }}")
                }
                scheduleReconnect(conversationId)
            }
        }
    }

    private companion object {
        const val LatestMessageLimit = 30
    }
}

data class ChatUiState(
    val conversationId: String = "",
    val isLoading: Boolean = false,
    val isSending: Boolean = false,
    val messages: List<ChatMessage> = emptyList(),
    val loadedOffset: Int = 0,
    val totalMessages: Int = 0,
    val draft: String = "",
    val error: String? = null,
    val realtimeState: String = "",
    val progressTitle: String? = null,
    val progressDetail: String? = null,
    val progressImportant: Boolean = false,
    val attachmentPreviews: Map<String, AttachmentPreviewUiState> = emptyMap(),
)

data class AttachmentPreviewUiState(
    val isLoading: Boolean = false,
    val error: String? = null,
    val image: Bitmap? = null,
    val text: String? = null,
    val detail: String? = null,
) {
    val hasContent: Boolean = image != null || text != null || detail != null
}

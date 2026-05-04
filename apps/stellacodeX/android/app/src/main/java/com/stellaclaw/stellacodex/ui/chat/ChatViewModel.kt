package com.stellaclaw.stellacodex.ui.chat

import android.app.Application
import android.content.ContentResolver
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import android.provider.OpenableColumns
import android.util.Base64
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.stellaclaw.stellacodex.core.result.AppResult
import com.stellaclaw.stellacodex.core.result.userMessage
import com.stellaclaw.stellacodex.data.api.MessagePage
import com.stellaclaw.stellacodex.data.api.StellaclawApi
import com.stellaclaw.stellacodex.data.dto.MessagesResponseDto
import com.stellaclaw.stellacodex.data.dto.SendMessageFileDto
import com.stellaclaw.stellacodex.data.log.AppLogStore
import com.stellaclaw.stellacodex.data.mapper.toDomain
import com.stellaclaw.stellacodex.data.store.ConnectionProfileStore
import com.stellaclaw.stellacodex.data.store.connectionDataStore
import com.stellaclaw.stellacodex.domain.model.ChatMessage
import com.stellaclaw.stellacodex.domain.model.ConnectionProfile
import com.stellaclaw.stellacodex.domain.model.MessageAttachment
import com.stellaclaw.stellacodex.domain.model.MessageItem
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
    private var realtimeSyncJob: Job? = null
    private var realtimeSyncInFlight: Boolean = false
    private var reconnectAttempt: Int = 0
    private var reconnectEnabled: Boolean = false
    private var latestProfile: ConnectionProfile? = null
    private var loadRequestSeq: Long = 0
    private var sawActiveTurnProgress: Boolean = false

    fun load(conversationId: String) {
        if (conversationId.isBlank()) return
        if (state.value.conversationId == conversationId && webSocket != null) return
        val requestSeq = ++loadRequestSeq
        cacheCurrentConversation()
        closeRealtime(allowReconnect = false)
        mutableState.update {
            it.copy(
                conversationId = conversationId,
                displayName = conversationId,
                messages = emptyList(),
                loadedOffset = 0,
                totalMessages = 0,
                isLoading = true,
                realtimeState = "Connecting realtime...",
                progressTitle = null,
                progressDetail = null,
            )
        }
        viewModelScope.launch {
            val profile = store.profile.first()
            if (requestSeq != loadRequestSeq || state.value.conversationId != conversationId) return@launch
            latestProfile = profile
            val cached = ConversationRuntimeCache.get(profile, conversationId)
            if (cached != null) {
                mutableState.update {
                    it.copy(
                        displayName = cached.displayName.ifBlank { conversationId },
                        isLoading = false,
                        messages = cached.messages,
                        loadedOffset = cached.loadedOffset,
                        totalMessages = cached.totalMessages,
                        error = null,
                    )
                }
                connectRealtime(profile, conversationId)
                markConversationSeen(profile, conversationId, cached.totalMessages)
            } else {
                refresh(connectRealtimeAfterLoad = true)
            }
        }
    }

    fun onDraftChanged(value: String) {
        mutableState.update { it.copy(draft = value, error = null) }
    }

    fun addAttachments(uris: List<Uri>) {
        if (uris.isEmpty()) return
        val resolver = getApplication<Application>().contentResolver
        val incoming = uris.map { uri -> pendingAttachmentFromUri(uri, resolver) }
        mutableState.update { state ->
            val existingUris = state.pendingAttachments.map { it.uri }.toSet()
            val merged = state.pendingAttachments + incoming.filterNot { it.uri in existingUris }
            val totalBytes = merged.sumOf { it.sizeBytes ?: 0L }
            if (totalBytes > MaxAttachmentBytes) {
                state.copy(error = "Attachments are limited to ${formatBytes(MaxAttachmentBytes)} total")
            } else {
                state.copy(pendingAttachments = merged, error = null)
            }
        }
    }

    fun removeAttachment(uri: String) {
        mutableState.update {
            it.copy(pendingAttachments = it.pendingAttachments.filterNot { attachment -> attachment.uri == uri })
        }
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
        val requestSeq = loadRequestSeq
        viewModelScope.launch {
            if (showLoading) {
                mutableState.update { it.copy(isLoading = true, error = null) }
            } else {
                mutableState.update { it.copy(error = null) }
            }
            val profile = store.profile.first()
            latestProfile = profile
            updateConversationTitle(profile, conversationId)
            when (val result = loadLatestVisibleMessages(profile, conversationId)) {
                is AppResult.Ok -> {
                    if (requestSeq != loadRequestSeq || state.value.conversationId != conversationId) return@launch
                    mutableState.update {
                        it.copy(
                            isLoading = false,
                            messages = mergeMessages(it.messages, result.value.messages),
                            loadedOffset = result.value.offset,
                            totalMessages = result.value.total,
                            error = null,
                        )
                    }
                    markConversationSeen(profile, conversationId, result.value.total)
                    cacheCurrentConversation()
                    if (connectRealtimeAfterLoad) {
                        connectRealtime(profile, conversationId)
                    }
                }
                is AppResult.Err -> {
                    if (requestSeq != loadRequestSeq || state.value.conversationId != conversationId) return@launch
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

    private suspend fun loadLatestVisibleMessages(
        profile: ConnectionProfile,
        conversationId: String,
    ): AppResult<MessagePage> {
        val firstPage = when (val result = api.loadMessagePage(profile, conversationId, offset = 0, limit = 1)) {
            is AppResult.Err -> return result
            is AppResult.Ok -> result.value
        }
        val total = firstPage.total
        if (total == 0) return AppResult.Ok(firstPage)

        var nextEnd = total
        var loadedOffset = total
        var loadedMessages = emptyList<ChatMessage>()
        while (nextEnd > 0 && loadedMessages.visibleTimelineItemCount() < VisibleTimelineItemTarget) {
            val nextOffset = (nextEnd - MessagePageFetchLimit).coerceAtLeast(0)
            val page = when (val result = api.loadMessagePage(
                profile = profile,
                conversationId = conversationId,
                offset = nextOffset,
                limit = nextEnd - nextOffset,
            )) {
                is AppResult.Err -> return result
                is AppResult.Ok -> result.value
            }
            loadedMessages = mergeMessages(page.messages, loadedMessages)
            loadedOffset = page.offset
            nextEnd = nextOffset
            if (page.messages.isEmpty()) break
        }
        return AppResult.Ok(
            MessagePage(
                offset = loadedOffset,
                limit = loadedMessages.size,
                total = total,
                messages = loadedMessages,
            ),
        )
    }

    private suspend fun loadEarlierVisibleMessages(
        profile: ConnectionProfile,
        conversationId: String,
        startOffset: Int,
    ): AppResult<MessagePage> {
        var nextEnd = startOffset
        var loadedOffset = startOffset
        var loadedMessages = emptyList<ChatMessage>()
        var total = state.value.totalMessages
        while (nextEnd > 0 && loadedMessages.visibleTimelineItemCount() < VisibleTimelineItemTarget) {
            val nextOffset = (nextEnd - MessagePageFetchLimit).coerceAtLeast(0)
            val page = when (val result = api.loadMessagePage(
                profile = profile,
                conversationId = conversationId,
                offset = nextOffset,
                limit = nextEnd - nextOffset,
            )) {
                is AppResult.Err -> return result
                is AppResult.Ok -> result.value
            }
            loadedMessages = mergeMessages(page.messages, loadedMessages)
            loadedOffset = page.offset
            total = page.total
            nextEnd = nextOffset
            if (page.messages.isEmpty()) break
        }
        return AppResult.Ok(
            MessagePage(
                offset = loadedOffset,
                limit = loadedMessages.size,
                total = total,
                messages = loadedMessages,
            ),
        )
    }

    fun loadEarlier() {
        val snapshot = state.value
        val conversationId = snapshot.conversationId
        if (conversationId.isBlank() || snapshot.loadedOffset <= 0 || snapshot.isLoadingEarlier) return
        viewModelScope.launch {
            mutableState.update { it.copy(isLoadingEarlier = true, error = null) }
            val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
            when (val result = loadEarlierVisibleMessages(profile, conversationId, snapshot.loadedOffset)) {
                is AppResult.Ok -> {
                    mutableState.update {
                        it.copy(
                            isLoadingEarlier = false,
                            messages = mergeMessages(result.value.messages, it.messages),
                            loadedOffset = result.value.offset,
                            totalMessages = result.value.total,
                            error = null,
                        )
                    }
                    cacheCurrentConversation()
                }
                is AppResult.Err -> mutableState.update {
                    it.copy(isLoadingEarlier = false, error = result.error.userMessage())
                }
            }
        }
    }

    fun send() {
        val current = state.value
        val text = current.draft.trim()
        val attachments = current.pendingAttachments
        if (current.conversationId.isBlank() || (text.isEmpty() && attachments.isEmpty()) || current.isSending) return
        val baselineIndex = lastServerMessageIndex(current.messages) ?: -1
        val localId = "local-${System.currentTimeMillis()}"
        val senderName = latestProfile?.userName?.ifBlank { "workspace-user" } ?: "workspace-user"
        val optimistic = ChatMessage(
            id = localId,
            index = nextLocalIndex(current.messages),
            role = "user",
            text = text,
            preview = text,
            userName = senderName,
            messageTime = Instant.now().toString(),
            attachmentCount = attachments.size,
            attachments = emptyList(),
            items = emptyList(),
            hasAttachmentErrors = false,
            hasTokenUsage = false,
            localState = MessageLocalState.Sending,
        )
        mutableState.update {
            it.copy(
                draft = "",
                pendingAttachments = emptyList(),
                isSending = true,
                error = null,
                messages = it.messages + optimistic,
            )
        }
        cacheCurrentConversation()
        viewModelScope.launch {
            val profile: ConnectionProfile = store.profile.first()
            latestProfile = profile
            val files = try {
                attachments.map { it.toSendFile() }
            } catch (error: Exception) {
                mutableState.update { state ->
                    state.copy(
                        isSending = false,
                        error = "Failed to read attachment: ${error.message.orEmpty()}",
                        pendingAttachments = attachments,
                        messages = state.messages.filterNot { message -> message.id == localId },
                    )
                }
                return@launch
            }
            when (val result = api.sendMessage(profile, current.conversationId, text, files)) {
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
                    cacheCurrentConversation()
                    AgentCompletionService.watch(
                        context = getApplication<Application>(),
                        conversationId = current.conversationId,
                        baselineIndex = baselineIndex,
                    )
                    delay(350)
                    refresh()
                }
                is AppResult.Err -> mutableState.update { state ->
                    state.copy(
                        isSending = false,
                        error = result.error.userMessage(),
                        pendingAttachments = attachments,
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
        logRealtime("connect requested conversation=$conversationId mode=${profile.connectionMode}")
        when (val result = api.foregroundWebSocketRequest(profile, conversationId)) {
            is AppResult.Ok -> {
                realtimeConversationId = conversationId
                startRealtimeBackfill(conversationId)
                webSocket?.cancel()
                val request = result.value
                logRealtime("opening websocket conversation=$conversationId url=${request.url.scheme}://${request.url.host}:${request.url.port}${request.url.encodedPath}?token=<redacted>")
                webSocket = api.client.newWebSocket(request, ChatWebSocketListener(conversationId))
            }
            is AppResult.Err -> {
                val message = result.error.userMessage()
                logRealtime("websocket request failed conversation=$conversationId error=$message")
                mutableState.update { it.copy(realtimeState = message) }
                scheduleReconnect(conversationId)
            }
        }
    }

    private fun scheduleReconnect(conversationId: String) {
        if (!reconnectEnabled || conversationId != realtimeConversationId && realtimeConversationId.isNotBlank()) return
        reconnectJob?.cancel()
        val delayMillis = min(30_000L, 1_000L * (1 shl min(reconnectAttempt, 5)))
        reconnectAttempt += 1
        logRealtime("schedule reconnect conversation=$conversationId delay=${delayMillis}ms attempt=$reconnectAttempt")
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
        logRealtime("close websocket allowReconnect=$allowReconnect conversation=$realtimeConversationId")
        if (!allowReconnect) {
            realtimeSyncJob?.cancel()
            realtimeSyncJob = null
            realtimeSyncInFlight = false
        }
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
            current.copy(messages = mergeMessages(current.messages, incoming))
        }
        cacheCurrentConversation()
    }

    private fun cacheCurrentConversation() {
        val profile = latestProfile ?: return
        val snapshot = state.value
        if (snapshot.conversationId.isBlank()) return
        ConversationRuntimeCache.put(
            profile = profile,
            conversationId = snapshot.conversationId,
            snapshot = CachedChatSnapshot(
                displayName = snapshot.displayName,
                messages = snapshot.messages,
                loadedOffset = snapshot.loadedOffset,
                totalMessages = snapshot.totalMessages,
            ),
        )
    }

    private fun List<ChatMessage>.visibleTimelineItemCount(): Int {
        var count = 0
        var pendingToolMessageCount = 0
        for (message in filterNot { it.isRuntimeMetadataMessage() }) {
            if (message.isToolOnlyMessage()) {
                pendingToolMessageCount += 1
            } else {
                if (pendingToolMessageCount > 0) {
                    count += 1
                    pendingToolMessageCount = 0
                }
                count += 1
            }
        }
        count += pendingToolMessageCount
        return count
    }

    private fun ChatMessage.isToolOnlyMessage(): Boolean =
        role.equals("assistant", ignoreCase = true) &&
            text.isBlank() &&
            attachments.isEmpty() &&
            items.any { it is MessageItem.ToolCall || it is MessageItem.ToolResult }

    private fun ChatMessage.isRuntimeMetadataMessage(): Boolean {
        val body = text.ifBlank { preview }.trimStart()
        return body.startsWith("[Incoming User Metadata]") ||
            body.startsWith("[Incoming Assistant Metadata]") ||
            body.startsWith("[Incoming System Metadata]")
    }

    private fun mergeMessages(existing: List<ChatMessage>, incoming: List<ChatMessage>): List<ChatMessage> {
        val existingById = existing.associateBy { it.id }
        val receivedAt = Instant.now().toString()
        val syncedIncoming = incoming.map { remote ->
            val preservedTime = existingById[remote.id]?.messageTime
            val clientReceivedTime = remote.role
                .takeIf { it.equals("assistant", ignoreCase = true) }
                ?.takeIf { remote.messageTime.isNullOrBlank() }
                ?.let { preservedTime ?: receivedAt }
            remote.copy(
                localState = MessageLocalState.Synced,
                messageTime = remote.messageTime ?: clientReceivedTime,
            )
        }
        val byId = linkedMapOf<String, ChatMessage>()
        existing.filterNot { local ->
            local.localState == MessageLocalState.Sending && syncedIncoming.any { remote ->
                remote.role == local.role && remote.text == local.text && remote.userName == local.userName
            }
        }.forEach { byId[it.id] = it }
        syncedIncoming.forEach { byId[it.id] = it }
        return byId.values.sortedWith(compareBy<ChatMessage> { it.index }.thenBy { it.id })
    }

    private suspend fun updateConversationTitle(profile: ConnectionProfile, conversationId: String) {
        when (val result = api.loadConversations(profile, limit = 200)) {
            is AppResult.Ok -> {
                val displayName = result.value
                    .firstOrNull { it.conversationId == conversationId }
                    ?.displayName
                    ?.takeIf { it.isNotBlank() }
                    ?: conversationId
                mutableState.update { it.copy(displayName = displayName) }
            }
            is AppResult.Err -> Unit
        }
    }

    private fun markConversationSeen(profile: ConnectionProfile, conversationId: String, totalMessages: Int) {
        val lastSeenMessageId = totalMessages.minus(1).takeIf { it >= 0 }?.toString() ?: return
        viewModelScope.launch {
            when (val result = api.markConversationSeen(profile, conversationId, lastSeenMessageId)) {
                is AppResult.Ok -> Unit
                is AppResult.Err -> logRealtime("mark seen failed conversation=$conversationId error=${result.error.userMessage()}")
            }
        }
    }

    private fun lastServerMessageIndex(messages: List<ChatMessage>): Int? = messages
        .asReversed()
        .firstOrNull { it.localState == MessageLocalState.Synced && it.id.toIntOrNull() != null }
        ?.id
        ?.toIntOrNull()

    private fun startRealtimeBackfill(conversationId: String) {
        realtimeSyncJob?.cancel()
        realtimeSyncJob = viewModelScope.launch {
            while (state.value.conversationId == conversationId && reconnectEnabled) {
                delay(2_000)
                val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
                syncMissingMessages(conversationId, profile, updateStatusWhenIdle = false)
            }
        }
    }

    private suspend fun syncMissingMessages(
        conversationId: String,
        profile: ConnectionProfile,
        updateStatusWhenIdle: Boolean,
    ) {
        if (realtimeSyncInFlight || state.value.conversationId != conversationId) return
        realtimeSyncInFlight = true
        try {
            val snapshot = state.value
            val lastLocalId = lastServerMessageIndex(snapshot.messages)
            val request = if (lastLocalId == null) {
                0 to MessagePageFetchLimit
            } else {
                (lastLocalId + 1) to 200
            }
            when (val result = api.loadMessagePage(profile, conversationId, offset = request.first, limit = request.second)) {
                is AppResult.Ok -> {
                    val page = result.value
                    val shouldLoadLatest = lastLocalId == null && page.total > page.messages.size
                    if (shouldLoadLatest) {
                        val latestOffset = (page.total - MessagePageFetchLimit).coerceAtLeast(0)
                        when (val latest = api.loadMessagePage(profile, conversationId, offset = latestOffset, limit = MessagePageFetchLimit)) {
                            is AppResult.Ok -> mutableState.update {
                                it.copy(
                                    messages = mergeMessages(it.messages, latest.value.messages),
                                    loadedOffset = latest.value.offset,
                                    totalMessages = latest.value.total,
                                    error = null,
                                    realtimeState = "Realtime synced",
                                )
                            }
                            is AppResult.Err -> Unit
                        }
                    } else if (page.messages.isNotEmpty() || page.total != snapshot.totalMessages) {
                        mutableState.update {
                            it.copy(
                                messages = mergeMessages(it.messages, page.messages),
                                loadedOffset = if (page.messages.isNotEmpty()) min(it.loadedOffset, page.offset) else it.loadedOffset,
                                totalMessages = page.total,
                                error = null,
                                realtimeState = if (page.messages.isNotEmpty()) {
                                    "Realtime synced · ${page.offset + page.messages.size}/${page.total}"
                                } else {
                                    it.realtimeState
                                },
                            )
                        }
                    } else if (updateStatusWhenIdle) {
                        mutableState.update { it.copy(totalMessages = page.total, realtimeState = "Realtime synced") }
                    }
                    cacheCurrentConversation()
                }
                is AppResult.Err -> if (updateStatusWhenIdle) {
                    mutableState.update { it.copy(realtimeState = "Realtime sync failed: ${result.error.userMessage()}") }
                }
            }
        } finally {
            realtimeSyncInFlight = false
        }
    }

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
                        val offset = (latestIndex - MessagePageFetchLimit + 1).coerceAtLeast(0)
                        offset to MessagePageFetchLimit
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
                            messages = mergeMessages(it.messages, result.value.messages),
                            loadedOffset = min(it.loadedOffset, result.value.offset),
                            totalMessages = result.value.total,
                            error = null,
                            realtimeState = "Realtime synced",
                        )
                    }
                    cacheCurrentConversation()
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

    private fun PendingAttachmentUiState.toSendFile(): SendMessageFileDto {
        val resolver = getApplication<Application>().contentResolver
        val uriValue = Uri.parse(uri)
        val bytes = resolver.openInputStream(uriValue)?.use { it.readBytes() }
            ?: throw IllegalArgumentException("cannot open $name")
        if (bytes.size > MaxAttachmentBytes) {
            throw IllegalArgumentException("${name} is larger than ${formatBytes(MaxAttachmentBytes)}")
        }
        val base64 = Base64.encodeToString(bytes, Base64.NO_WRAP)
        val media = mediaType ?: resolver.getType(uriValue) ?: "application/octet-stream"
        return SendMessageFileDto(
            uri = "data:$media;base64,$base64",
            mediaType = media,
            name = name,
        )
    }

    private fun pendingAttachmentFromUri(uri: Uri, resolver: ContentResolver): PendingAttachmentUiState {
        var name = uri.lastPathSegment?.substringAfterLast('/')?.ifBlank { null } ?: "attachment"
        var size: Long? = null
        resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE), null, null, null)?.use { cursor ->
            if (cursor.moveToFirst()) {
                val nameIndex = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                if (nameIndex >= 0) {
                    name = cursor.getString(nameIndex)?.takeIf { it.isNotBlank() } ?: name
                }
                val sizeIndex = cursor.getColumnIndex(OpenableColumns.SIZE)
                if (sizeIndex >= 0 && !cursor.isNull(sizeIndex)) {
                    size = cursor.getLong(sizeIndex)
                }
            }
        }
        return PendingAttachmentUiState(
            uri = uri.toString(),
            name = name,
            mediaType = resolver.getType(uri),
            sizeBytes = size,
        )
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

    private fun logRealtime(message: String) {
        AppLogStore.append(getApplication(), "realtime", message)
    }

    private fun handleWebSocketText(conversationId: String, text: String) {
        if (conversationId != realtimeConversationId) return
        try {
            val payload = json.decodeFromString<JsonObject>(text)
            when (payload["type"]?.jsonPrimitive?.content) {
                "subscription_ack" -> {
                    val reason = payload["reason"]?.jsonPrimitive?.content.orEmpty()
                    logRealtime("subscription_ack conversation=$conversationId reason=$reason")
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
                    viewModelScope.launch {
                        val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
                        syncMissingMessages(conversationId, profile, updateStatusWhenIdle = true)
                    }
                }
                "messages" -> {
                    val page = json.decodeFromString<MessagesResponseDto>(text)
                    val messages = page.messages.map { it.toDomain() }
                    logRealtime("messages frame conversation=$conversationId count=${messages.size} offset=${page.offset} total=${page.total}")
                    applyIncomingMessages(messages)
                    latestProfile?.let { profile -> markConversationSeen(profile, conversationId, page.total) }
                    reconnectAttempt = 0
                    mutableState.update {
                        it.copy(
                            loadedOffset = page.offset,
                            totalMessages = page.total,
                            realtimeState = "Realtime connected · ${page.offset + messages.size}/${page.total}",
                        )
                    }
                    cacheCurrentConversation()
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
        } catch (error: SerializationException) {
            logRealtime("malformed frame conversation=$conversationId error=${error.message.orEmpty()} text=${text.take(240)}")
        } catch (error: IllegalArgumentException) {
            logRealtime("unexpected frame conversation=$conversationId error=${error.message.orEmpty()} text=${text.take(240)}")
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
        if (finalState == null) {
            sawActiveTurnProgress = true
            AgentCompletionService.watch(
                context = getApplication<Application>(),
                conversationId = state.value.conversationId,
                baselineIndex = lastServerMessageIndex(state.value.messages) ?: -1,
            )
        }
        mutableState.update {
            it.copy(
                progressTitle = title,
                progressDetail = detail,
                progressImportant = important,
                realtimeState = if (finalState == null) "Realtime active" else "Realtime connected",
            )
        }
        if (finalState == "done" || finalState == "failed") {
            val shouldNotify = finalState == "done" && sawActiveTurnProgress
            sawActiveTurnProgress = false
            viewModelScope.launch {
                delay(500)
                val conversationId = state.value.conversationId
                val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
                syncMissingMessages(conversationId, profile, updateStatusWhenIdle = true)
                if (shouldNotify) {
                    val snapshot = state.value
                    val latestAssistant = snapshot.messages.lastOrNull { message ->
                        message.role.equals("assistant", ignoreCase = true) &&
                            !message.isToolOnlyMessage() &&
                            !message.isRuntimeMetadataMessage()
                    }
                    val completionKey = latestAssistant?.let { "$conversationId:${it.index}" }
                    AgentNotificationCenter.notifyAgentDone(
                        context = getApplication<Application>(),
                        conversationId = snapshot.conversationId,
                        title = "Agent finished",
                        detail = latestAssistant?.text?.ifBlank { latestAssistant.preview }?.take(160),
                        completionKey = completionKey,
                    )
                    AgentCompletionService.stop(getApplication<Application>(), conversationId)
                }
                delay(1200)
                mutableState.update { it.copy(progressTitle = null, progressDetail = null, progressImportant = false) }
            }
        }
    }

    override fun onCleared() {
        cacheCurrentConversation()
        closeRealtime(allowReconnect = false)
        super.onCleared()
    }

    private fun websocketCloseMessage(prefix: String, code: Int, reason: String): String =
        if (reason.isBlank()) "$prefix: $code" else "$prefix: $code · $reason"

    private fun realtimeFailureMessage(error: Throwable, response: Response?): String {
        val errorName = error::class.simpleName ?: "Error"
        val message = error.message.orEmpty().ifBlank { "unknown" }
        val status = response?.let { " · HTTP ${it.code}${it.message.ifBlank { "" }.let { value -> if (value.isBlank()) "" else " ${value}" }}" }.orEmpty()
        return "$errorName: $message$status"
    }

    private inner class ChatWebSocketListener(
        private val conversationId: String,
    ) : WebSocketListener() {
        override fun onOpen(webSocket: WebSocket, response: Response) {
            reconnectAttempt = 0
            logRealtime("onOpen conversation=$conversationId http=${response.code} ${response.message}")
            mutableState.update { it.copy(realtimeState = "Realtime connected") }
        }

        override fun onMessage(webSocket: WebSocket, text: String) {
            handleWebSocketText(conversationId, text)
        }

        override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
            val message = websocketCloseMessage("Realtime closing", code, reason)
            logRealtime("onClosing conversation=$conversationId $message")
            mutableState.update { it.copy(realtimeState = message) }
            webSocket.close(code, reason)
        }

        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
            if (conversationId == realtimeConversationId && reconnectEnabled) {
                val message = websocketCloseMessage("Realtime closed", code, reason)
                logRealtime("onClosed conversation=$conversationId $message")
                mutableState.update { it.copy(realtimeState = message) }
                scheduleReconnect(conversationId)
            }
        }

        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
            if (conversationId == realtimeConversationId && reconnectEnabled) {
                val message = realtimeFailureMessage(t, response)
                logRealtime("onFailure conversation=$conversationId $message")
                mutableState.update {
                    it.copy(realtimeState = "Realtime error: $message")
                }
                scheduleReconnect(conversationId)
            }
        }
    }

    private companion object {
        const val VisibleTimelineItemTarget = 20
        const val MessagePageFetchLimit = 20
        const val MaxAttachmentBytes = 8L * 1024L * 1024L
    }
}

data class ChatUiState(
    val conversationId: String = "",
    val displayName: String = "",
    val isLoading: Boolean = false,
    val isLoadingEarlier: Boolean = false,
    val isSending: Boolean = false,
    val messages: List<ChatMessage> = emptyList(),
    val loadedOffset: Int = 0,
    val totalMessages: Int = 0,
    val pendingAttachments: List<PendingAttachmentUiState> = emptyList(),
    val draft: String = "",
    val error: String? = null,
    val realtimeState: String = "",
    val progressTitle: String? = null,
    val progressDetail: String? = null,
    val progressImportant: Boolean = false,
    val attachmentPreviews: Map<String, AttachmentPreviewUiState> = emptyMap(),
)

data class PendingAttachmentUiState(
    val uri: String,
    val name: String,
    val mediaType: String? = null,
    val sizeBytes: Long? = null,
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

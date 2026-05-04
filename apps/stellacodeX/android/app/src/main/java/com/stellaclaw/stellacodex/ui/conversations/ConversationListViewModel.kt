package com.stellaclaw.stellacodex.ui.conversations

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.stellaclaw.stellacodex.core.result.AppResult
import com.stellaclaw.stellacodex.core.result.userMessage
import com.stellaclaw.stellacodex.data.api.StellaclawApi
import com.stellaclaw.stellacodex.data.dto.ConversationSummaryDto
import com.stellaclaw.stellacodex.data.dto.ConversationsResponseDto
import com.stellaclaw.stellacodex.data.log.AppLogStore
import com.stellaclaw.stellacodex.data.mapper.toDomain
import com.stellaclaw.stellacodex.data.network.NetworkMonitor
import com.stellaclaw.stellacodex.data.network.NetworkState
import com.stellaclaw.stellacodex.data.store.ConnectionProfileStore
import com.stellaclaw.stellacodex.data.store.connectionDataStore
import com.stellaclaw.stellacodex.domain.model.ConnectionMode
import com.stellaclaw.stellacodex.domain.model.ConnectionProfile
import com.stellaclaw.stellacodex.domain.model.ConversationSummary
import kotlinx.coroutines.CoroutineExceptionHandler
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
import kotlinx.serialization.json.decodeFromJsonElement
import kotlinx.serialization.json.jsonPrimitive
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import kotlin.math.min
import kotlin.random.Random

class ConversationListViewModel(application: Application) : AndroidViewModel(application) {
    private val store = ConnectionProfileStore(application.connectionDataStore)
    private val api = StellaclawApi()
    private val json = Json {
        ignoreUnknownKeys = true
        explicitNulls = false
    }

    private val mutableState = MutableStateFlow(ConversationListUiState())
    val state: StateFlow<ConversationListUiState> = mutableState.asStateFlow()
    private val coroutineErrorHandler = CoroutineExceptionHandler { _, throwable ->
        log("coroutine failure ${throwable::class.java.simpleName}: ${throwable.message.orEmpty()}")
        mutableState.update { it.copy(isLoading = false, isCreating = false, error = throwable.message ?: "Unexpected app error") }
    }
    private var streamSocket: WebSocket? = null
    private var streamReconnectJob: Job? = null
    private var periodicRefreshJob: Job? = null
    private var reconnectAttempt = 0
    private var latestProfile: ConnectionProfile? = null

    init {
        log("ConversationListViewModel.init")
        NetworkMonitor.start(application)
        refresh()
        viewModelScope.launch(coroutineErrorHandler) {
            NetworkMonitor.state.collect { networkState ->
                when (networkState) {
                    NetworkState.Available -> {
                        if (state.value.error != null) {
                            log("network restored; refresh conversations after previous error")
                            refresh(showLoading = false)
                        }
                        val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
                        if (profile.connectionMode == ConnectionMode.SshProxy) api.invalidateTunnel()
                        if (streamSocket == null) connectConversationStream(profile, forceRefreshTunnel = true)
                    }
                    NetworkState.Lost, NetworkState.Unavailable -> log("network unavailable; stream waits for resume")
                }
            }
        }
    }

    fun refresh(showLoading: Boolean = true) {
        log("refresh showLoading=$showLoading")
        viewModelScope.launch(coroutineErrorHandler) {
            if (showLoading) {
                mutableState.update { it.copy(isLoading = true, error = null) }
            } else {
                mutableState.update { it.copy(error = null) }
            }
            val profile = store.profile.first()
            latestProfile = profile
            if (!profile.isConfigured) {
                mutableState.update {
                    it.copy(
                        isLoading = false,
                        activeConnectionName = profile.name.ifBlank { "Stellaclaw" },
                        error = "Connection profile is incomplete. Go back to connection setup.",
                    )
                }
                closeConversationStream()
                return@launch
            }
            when (val result = api.loadConversations(profile)) {
                is AppResult.Ok -> {
                    mutableState.update {
                        it.copy(
                            isLoading = false,
                            activeConnectionName = profile.displayName(),
                            conversations = result.value,
                            error = null,
                        )
                    }
                    connectConversationStream(profile)
                    ensurePeriodicRefresh(profile)
                }
                is AppResult.Err -> mutableState.update {
                    it.copy(
                        isLoading = false,
                        activeConnectionName = profile.displayName(),
                        error = result.error.userMessage(),
                    )
                }
            }
        }
    }

    fun refreshOnResume() {
        refresh(showLoading = false)
    }

    fun createConversation() {
        log("create conversation requested")
        if (state.value.isCreating) return
        viewModelScope.launch(coroutineErrorHandler) {
            mutableState.update { it.copy(isCreating = true, error = null) }
            val profile = store.profile.first()
            latestProfile = profile
            if (!profile.isConfigured) {
                mutableState.update {
                    it.copy(
                        isCreating = false,
                        error = "Connection profile is incomplete. Go back to connection setup.",
                    )
                }
                return@launch
            }
            when (val result = api.createConversation(profile)) {
                is AppResult.Ok -> {
                    mutableState.update {
                        it.copy(
                            isCreating = false,
                            pendingOpenConversationId = result.value,
                        )
                    }
                    refresh()
                }
                is AppResult.Err -> mutableState.update {
                    it.copy(
                        isCreating = false,
                        error = result.error.userMessage(),
                    )
                }
            }
        }
    }

    fun consumePendingOpenConversation() {
        mutableState.update { it.copy(pendingOpenConversationId = null) }
    }

    override fun onCleared() {
        closeConversationStream()
        periodicRefreshJob?.cancel()
        super.onCleared()
    }

    private suspend fun connectConversationStream(
        profile: ConnectionProfile,
        forceRefreshTunnel: Boolean = false,
    ) {
        if (!profile.isConfigured || streamSocket != null) return
        latestProfile = profile
        streamReconnectJob?.cancel()
        when (val result = api.conversationStreamRequest(profile, forceRefreshTunnel = forceRefreshTunnel)) {
            is AppResult.Ok -> {
                val request = result.value
                log("opening conversation stream ${request.url.scheme}://${request.url.host}:${request.url.port}${request.url.encodedPath}")
                streamSocket = api.webSocketClient.newWebSocket(request, ConversationStreamListener())
            }
            is AppResult.Err -> {
                log("conversation stream request failed ${result.error.userMessage()}")
                scheduleStreamReconnect()
            }
        }
    }

    private fun closeConversationStream() {
        streamReconnectJob?.cancel()
        streamReconnectJob = null
        streamSocket?.close(1000, "conversation list closed")
        streamSocket = null
        reconnectAttempt = 0
    }

    private fun scheduleStreamReconnect() {
        if (!NetworkMonitor.isAvailable()) return
        streamReconnectJob?.cancel()
        val baseDelay = min(30_000L, 1_000L * (1 shl min(reconnectAttempt, 5)))
        val delayMillis = (baseDelay * Random.nextDouble(0.8, 1.2)).toLong().coerceAtLeast(500L)
        reconnectAttempt += 1
        log("schedule conversation stream reconnect delay=${delayMillis}ms attempt=$reconnectAttempt")
        streamReconnectJob = viewModelScope.launch(coroutineErrorHandler) {
            delay(delayMillis)
            val profile = latestProfile ?: store.profile.first().also { latestProfile = it }
            streamSocket = null
            connectConversationStream(profile)
        }
    }

    private fun ensurePeriodicRefresh(profile: ConnectionProfile) {
        if (periodicRefreshJob?.isActive == true) return
        periodicRefreshJob = viewModelScope.launch(coroutineErrorHandler) {
            while (true) {
                delay(15_000L)
                if (!NetworkMonitor.isAvailable()) continue
                when (val result = api.loadConversations(profile)) {
                    is AppResult.Ok -> mutableState.update { it.copy(conversations = result.value, error = null) }
                    is AppResult.Err -> log("periodic refresh failed ${result.error.userMessage()}")
                }
            }
        }
    }

    private fun handleStreamText(text: String) {
        try {
            val payload = json.decodeFromString<JsonObject>(text)
            when (payload["type"]?.jsonPrimitive?.content) {
                "conversation_snapshot" -> {
                    val response = json.decodeFromString<ConversationsResponseDto>(text)
                    log("stream snapshot conversations=${response.conversations.size}")
                    mutableState.update { it.copy(conversations = response.conversations.map { item -> item.toDomain() }, error = null) }
                }
                "conversation_upserted", "conversation_turn_completed" -> {
                    val dto = payload["conversation"]?.let { json.decodeFromJsonElement<ConversationSummaryDto>(it) } ?: return
                    upsertConversation(dto.toDomain())
                }
                "conversation_processing" -> {
                    val conversationId = payload["conversation_id"]?.jsonPrimitive?.content ?: return
                    val running = payload["running"]?.jsonPrimitive?.content?.toBooleanStrictOrNull() ?: return
                    val processingState = payload["processing_state"]?.jsonPrimitive?.content ?: if (running) "running" else "idle"
                    patchConversation(conversationId) { it.copy(running = running, processingState = processingState) }
                }
                "conversation_seen" -> {
                    val conversationId = payload["conversation_id"]?.jsonPrimitive?.content ?: return
                    val seen = payload["seen"] as? JsonObject ?: return
                    val lastSeenMessageId = seen["last_seen_message_id"]?.jsonPrimitive?.content
                    val lastSeenAt = seen["updated_at"]?.jsonPrimitive?.content
                    patchConversation(conversationId) { it.copy(lastSeenMessageId = lastSeenMessageId, lastSeenAt = lastSeenAt) }
                }
            }
        } catch (error: SerializationException) {
            log("stream frame decode failed ${error.message.orEmpty()} text=${text.take(200)}")
        } catch (error: IllegalArgumentException) {
            log("stream frame unexpected ${error.message.orEmpty()} text=${text.take(200)}")
        }
    }

    private fun upsertConversation(summary: ConversationSummary) {
        mutableState.update { state ->
            val next = (state.conversations.filterNot { it.conversationId == summary.conversationId } + summary)
                .sortedBy { it.conversationId }
            state.copy(conversations = next, error = null)
        }
    }

    private fun patchConversation(
        conversationId: String,
        transform: (ConversationSummary) -> ConversationSummary,
    ) {
        mutableState.update { state ->
            state.copy(
                conversations = state.conversations.map { summary ->
                    if (summary.conversationId == conversationId) transform(summary) else summary
                },
                error = null,
            )
        }
    }

    private fun ConnectionProfile.displayName(): String = name.ifBlank { sshHost.ifBlank { baseUrl.ifBlank { "Stellaclaw" } } }

    private fun log(message: String) {
        AppLogStore.append(getApplication(), "conversations", message)
    }

    private inner class ConversationStreamListener : WebSocketListener() {
        override fun onOpen(webSocket: WebSocket, response: Response) {
            reconnectAttempt = 0
            log("conversation stream open http=${response.code} ${response.message}")
        }

        override fun onMessage(webSocket: WebSocket, text: String) {
            handleStreamText(text)
        }

        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
            if (streamSocket == webSocket) {
                streamSocket = null
                log("conversation stream closed code=$code reason=$reason")
                scheduleStreamReconnect()
            }
        }

        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
            if (streamSocket == webSocket) {
                streamSocket = null
                log("conversation stream failure ${t::class.java.simpleName}: ${t.message.orEmpty()} http=${response?.code ?: 0}")
                scheduleStreamReconnect()
            }
        }
    }
}

data class ConversationListUiState(
    val activeConnectionName: String = "Stellaclaw",
    val isLoading: Boolean = false,
    val isCreating: Boolean = false,
    val conversations: List<ConversationSummary> = emptyList(),
    val error: String? = null,
    val pendingOpenConversationId: String? = null,
)

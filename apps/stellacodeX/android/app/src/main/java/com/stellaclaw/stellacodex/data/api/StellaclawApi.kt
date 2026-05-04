package com.stellaclaw.stellacodex.data.api

import com.stellaclaw.stellacodex.core.result.AppError
import com.stellaclaw.stellacodex.core.result.AppResult
import com.stellaclaw.stellacodex.data.dto.ConversationsResponseDto
import com.stellaclaw.stellacodex.data.dto.CreateConversationRequestDto
import com.stellaclaw.stellacodex.data.dto.CreateConversationResponseDto
import com.stellaclaw.stellacodex.data.dto.MarkConversationSeenRequestDto
import com.stellaclaw.stellacodex.data.dto.MessagesResponseDto
import com.stellaclaw.stellacodex.data.dto.ModelsResponseDto
import com.stellaclaw.stellacodex.data.dto.SendMessageFileDto
import com.stellaclaw.stellacodex.data.dto.SendMessageRequestDto
import com.stellaclaw.stellacodex.data.dto.SendMessageResponseDto
import com.stellaclaw.stellacodex.data.mapper.toDomain
import com.stellaclaw.stellacodex.data.ssh.SshTunnelManager
import com.stellaclaw.stellacodex.domain.model.ChatMessage
import com.stellaclaw.stellacodex.domain.model.ConnectionMode
import com.stellaclaw.stellacodex.domain.model.ConnectionProfile
import com.stellaclaw.stellacodex.domain.model.ConversationSummary
import com.stellaclaw.stellacodex.domain.model.ModelInfo
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import kotlinx.serialization.SerializationException
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import java.io.IOException
import java.util.concurrent.TimeUnit
import kotlin.math.min
import kotlin.random.Random

data class MessagePage(
    val offset: Int,
    val limit: Int,
    val total: Int,
    val messages: List<ChatMessage>,
)

data class FetchedAttachment(
    val bytes: ByteArray,
    val mediaType: String?,
)

class StellaclawApi(
    private val httpClient: OkHttpClient = defaultHttpClient(),
    val webSocketClient: OkHttpClient = defaultWebSocketClient(),
    private val tunnelManager: SshTunnelManager = SshTunnelManager(),
    private val json: Json = Json {
        ignoreUnknownKeys = true
        explicitNulls = false
    },
) {
    suspend fun loadModels(profile: ConnectionProfile): AppResult<List<ModelInfo>> = get(profile, "/api/models") { body ->
        json.decodeFromString<ModelsResponseDto>(body).models.map { it.toDomain() }
    }

    suspend fun loadConversations(
        profile: ConnectionProfile,
        limit: Int = 80,
    ): AppResult<List<ConversationSummary>> = get(profile, "/api/conversations?limit=$limit") { body ->
        json.decodeFromString<ConversationsResponseDto>(body).conversations.map { it.toDomain() }
    }

    suspend fun createConversation(
        profile: ConnectionProfile,
        nickname: String? = null,
    ): AppResult<String> = post(
        profile = profile,
        path = "/api/conversations",
        body = json.encodeToString(
            CreateConversationRequestDto(
                nickname = nickname?.trim()?.takeIf { it.isNotEmpty() },
            ),
        ),
        retryPolicy = RetryPolicy.NoRetry,
    ) { responseBody ->
        json.decodeFromString<CreateConversationResponseDto>(responseBody).conversationId
    }

    suspend fun loadMessagePage(
        profile: ConnectionProfile,
        conversationId: String,
        offset: Int = 0,
        limit: Int = 80,
    ): AppResult<MessagePage> = get(
        profile = profile,
        path = "/api/conversations/$conversationId/messages?offset=$offset&limit=$limit",
    ) { body ->
        val page = json.decodeFromString<MessagesResponseDto>(body)
        MessagePage(
            offset = page.offset,
            limit = page.limit,
            total = page.total,
            messages = page.messages.map { it.toDomain() },
        )
    }

    suspend fun loadLatestMessages(
        profile: ConnectionProfile,
        conversationId: String,
        limit: Int = 30,
    ): AppResult<MessagePage> = when (val firstPage = loadMessagePage(profile, conversationId, offset = 0, limit = 1)) {
        is AppResult.Err -> firstPage
        is AppResult.Ok -> {
            val total = firstPage.value.total
            val latestOffset = (total - limit).coerceAtLeast(0)
            loadMessagePage(profile, conversationId, offset = latestOffset, limit = limit)
        }
    }

    suspend fun markConversationSeen(
        profile: ConnectionProfile,
        conversationId: String,
        lastSeenMessageId: String,
    ): AppResult<Unit> = post(
        profile = profile,
        path = "/api/conversations/$conversationId/seen",
        body = json.encodeToString(MarkConversationSeenRequestDto(lastSeenMessageId = lastSeenMessageId)),
        retryPolicy = RetryPolicy.Default,
    ) { Unit }

    suspend fun sendMessage(
        profile: ConnectionProfile,
        conversationId: String,
        text: String,
        files: List<SendMessageFileDto> = emptyList(),
        remoteMessageId: String? = null,
    ): AppResult<Unit> = post(
        profile = profile,
        path = "/api/conversations/$conversationId/messages",
        body = json.encodeToString(
            SendMessageRequestDto(
                userName = profile.userName.ifBlank { "workspace-user" },
                text = text,
                files = files,
                remoteMessageId = remoteMessageId,
            ),
        ),
        retryPolicy = if (remoteMessageId.isNullOrBlank()) RetryPolicy.NoRetry else RetryPolicy.Default,
    ) { responseBody ->
        json.decodeFromString<SendMessageResponseDto>(responseBody)
        Unit
    }

    suspend fun fetchAttachment(
        profile: ConnectionProfile,
        attachmentUrl: String,
    ): AppResult<FetchedAttachment> {
        if (!profile.isConfigured) return AppResult.Err(AppError.MissingConnection)
        if (attachmentUrl.isBlank()) return AppResult.Err(AppError.Network("Attachment URL is empty"))
        return fetchAttachmentBytes(profile, attachmentUrl)
    }

    private suspend fun fetchAttachmentBytes(
        profile: ConnectionProfile,
        attachmentUrl: String,
    ): AppResult<FetchedAttachment> = withContext(Dispatchers.IO) {
        var attempt = 0
        var forceRefreshTunnel = false
        while (true) {
            try {
                val baseUrl = resolveBaseUrl(profile, forceRefreshTunnel)
                val urlText = if (attachmentUrl.startsWith("http://") || attachmentUrl.startsWith("https://")) {
                    attachmentUrl
                } else {
                    baseUrl.plus(if (attachmentUrl.startsWith('/')) attachmentUrl else "/$attachmentUrl")
                }
                val url = urlText.toHttpUrlOrNull()
                    ?: return@withContext AppResult.Err(AppError.Network("Invalid attachment URL"))
                val request = Request.Builder()
                    .url(url)
                    .header("Authorization", "Bearer ${profile.token.trim()}")
                    .get()
                    .build()
                httpClient.newCall(request).execute().use { response ->
                    val bytes = response.body?.bytes() ?: ByteArray(0)
                    val result = when {
                        response.code == 401 -> AppResult.Err(AppError.Unauthorized())
                        !response.isSuccessful -> AppResult.Err(AppError.Server(response.code, response.message))
                        else -> AppResult.Ok(FetchedAttachment(bytes = bytes, mediaType = response.header("content-type")))
                    }
                    if (!result.shouldRetry(attempt)) return@withContext result
                }
            } catch (error: IOException) {
                if (profile.connectionMode == ConnectionMode.SshProxy) tunnelManager.invalidate()
                if (attempt >= RetryPolicy.Default.maxRetries) {
                    return@withContext AppResult.Err(AppError.Network(error.message.orEmpty()))
                }
            } catch (error: Exception) {
                return@withContext AppResult.Err(AppError.Unknown(error.message.orEmpty()))
            }
            attempt += 1
            forceRefreshTunnel = profile.connectionMode == ConnectionMode.SshProxy
            delay(retryDelayMillis(attempt))
        }
        AppResult.Err(AppError.Network("attachment fetch retry exhausted"))
    }

    suspend fun foregroundWebSocketRequest(
        profile: ConnectionProfile,
        conversationId: String,
        forceRefreshTunnel: Boolean = false,
    ): AppResult<Request> {
        if (!profile.isConfigured) return AppResult.Err(AppError.MissingConnection)
        return withContext(Dispatchers.IO) {
            try {
                val baseUrl = resolveBaseUrl(profile, forceRefreshTunnel)
                val httpUrl = baseUrl
                    .plus("/api/conversations/$conversationId/foreground/ws")
                    .toHttpUrlOrNull()
                    ?: return@withContext AppResult.Err(AppError.Network("Invalid WebSocket URL"))
                val httpUrlWithToken = httpUrl.newBuilder()
                    .addQueryParameter("token", profile.token.trim())
                    .build()
                val wsUrl = when (httpUrlWithToken.scheme) {
                    "https" -> httpUrlWithToken.toString().replaceFirst("https://", "wss://")
                    "http" -> httpUrlWithToken.toString().replaceFirst("http://", "ws://")
                    else -> return@withContext AppResult.Err(AppError.Network("Unsupported WebSocket scheme"))
                }
                AppResult.Ok(
                    Request.Builder()
                        .url(wsUrl)
                        .header("Authorization", "Bearer ${profile.token.trim()}")
                        .build(),
                )
            } catch (error: IOException) {
                if (profile.connectionMode == ConnectionMode.SshProxy) tunnelManager.invalidate()
                AppResult.Err(AppError.Network(error.message.orEmpty()))
            } catch (error: Exception) {
                AppResult.Err(AppError.Unknown(error.message.orEmpty()))
            }
        }
    }

    fun invalidateTunnel() {
        tunnelManager.invalidate()
    }

    private suspend fun <T> get(
        profile: ConnectionProfile,
        path: String,
        decode: (String) -> T,
    ): AppResult<T> = request(profile, path, method = "GET", body = null, retryPolicy = RetryPolicy.Default, decode = decode)

    private suspend fun <T> post(
        profile: ConnectionProfile,
        path: String,
        body: String,
        retryPolicy: RetryPolicy,
        decode: (String) -> T,
    ): AppResult<T> = request(profile, path, method = "POST", body = body, retryPolicy = retryPolicy, decode = decode)

    private suspend fun <T> request(
        profile: ConnectionProfile,
        path: String,
        method: String,
        body: String?,
        retryPolicy: RetryPolicy,
        absoluteOrRelativePath: Boolean = false,
        decode: (String) -> T,
    ): AppResult<T> {
        if (!profile.isConfigured) return AppResult.Err(AppError.MissingConnection)

        var attempt = 0
        var forceRefreshTunnel = false
        while (true) {
            val result = executeRequest(profile, path, method, body, forceRefreshTunnel, absoluteOrRelativePath, decode)
            if (!result.shouldRetry(attempt) || attempt >= retryPolicy.maxRetries) return result
            if (profile.connectionMode == ConnectionMode.SshProxy) {
                tunnelManager.invalidate()
                forceRefreshTunnel = true
            }
            attempt += 1
            delay(retryDelayMillis(attempt))
        }
    }

    private suspend fun <T> executeRequest(
        profile: ConnectionProfile,
        path: String,
        method: String,
        body: String?,
        forceRefreshTunnel: Boolean,
        absoluteOrRelativePath: Boolean,
        decode: (String) -> T,
    ): AppResult<T> = withContext(Dispatchers.IO) {
        try {
            val baseUrl = resolveBaseUrl(profile, forceRefreshTunnel)
            val urlText = if (absoluteOrRelativePath && (path.startsWith("http://") || path.startsWith("https://"))) {
                path
            } else {
                baseUrl.plus(if (path.startsWith('/')) path else "/$path")
            }
            val url = urlText.toHttpUrlOrNull()
                ?: return@withContext AppResult.Err(AppError.Network("Invalid server URL"))
            val builder = Request.Builder()
                .url(url)
                .header("Authorization", "Bearer ${profile.token.trim()}")
            if (method == "POST") {
                builder.post((body ?: "{}").toRequestBody(JsonMediaType))
            } else {
                builder.get()
            }
            httpClient.newCall(builder.build()).execute().use { response ->
                val responseBody = response.body?.string().orEmpty()
                when {
                    response.code == 401 -> AppResult.Err(AppError.Unauthorized())
                    !response.isSuccessful -> AppResult.Err(AppError.Server(response.code, responseBody.ifBlank { response.message }))
                    else -> AppResult.Ok(decode(responseBody))
                }
            }
        } catch (error: SerializationException) {
            AppResult.Err(AppError.Decode(error.message.orEmpty()))
        } catch (error: IOException) {
            if (profile.connectionMode == ConnectionMode.SshProxy) tunnelManager.invalidate()
            AppResult.Err(AppError.Network(error.message.orEmpty()))
        } catch (error: Exception) {
            AppResult.Err(AppError.Unknown(error.message.orEmpty()))
        }
    }

    private fun resolveBaseUrl(profile: ConnectionProfile, forceRefreshTunnel: Boolean = false): String = when (profile.connectionMode) {
        ConnectionMode.Direct -> profile.baseUrl.trim().trimEnd('/')
        ConnectionMode.SshProxy -> tunnelManager.resolveBaseUrl(profile, forceRefresh = forceRefreshTunnel).trimEnd('/')
    }

    private fun <T> AppResult<T>.shouldRetry(attempt: Int): Boolean {
        if (attempt >= RetryPolicy.Default.maxRetries) return false
        return when (this) {
            is AppResult.Ok -> false
            is AppResult.Err -> when (val error = error) {
                is AppError.Network -> true
                is AppError.Server -> error.code in RetryableStatusCodes
                else -> false
            }
        }
    }

    private fun retryDelayMillis(attempt: Int): Long {
        val base = min(4_000L, 500L * (1L shl (attempt - 1).coerceAtLeast(0)))
        val jitter = Random.nextDouble(0.8, 1.2)
        return (base * jitter).toLong().coerceAtLeast(250L)
    }

    private data class RetryPolicy(val maxRetries: Int) {
        companion object {
            val Default = RetryPolicy(maxRetries = 3)
            val NoRetry = RetryPolicy(maxRetries = 0)
        }
    }

    private companion object {
        val JsonMediaType = "application/json; charset=utf-8".toMediaType()
        val RetryableStatusCodes = setOf(408, 429, 500, 502, 503, 504)

        fun defaultHttpClient(): OkHttpClient = OkHttpClient.Builder()
            .connectTimeout(10, TimeUnit.SECONDS)
            .readTimeout(30, TimeUnit.SECONDS)
            .writeTimeout(30, TimeUnit.SECONDS)
            .callTimeout(45, TimeUnit.SECONDS)
            .build()

        fun defaultWebSocketClient(): OkHttpClient = OkHttpClient.Builder()
            .connectTimeout(10, TimeUnit.SECONDS)
            .readTimeout(0, TimeUnit.SECONDS)
            .writeTimeout(20, TimeUnit.SECONDS)
            .pingInterval(15, TimeUnit.SECONDS)
            .build()
    }
}

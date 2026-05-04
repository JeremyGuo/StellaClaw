package com.stellaclaw.stellacodex.ui.chat

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import com.stellaclaw.stellacodex.MainActivity
import com.stellaclaw.stellacodex.R
import com.stellaclaw.stellacodex.core.result.AppResult
import com.stellaclaw.stellacodex.data.api.StellaclawApi
import com.stellaclaw.stellacodex.data.log.AppLogStore
import com.stellaclaw.stellacodex.data.network.NetworkMonitor
import com.stellaclaw.stellacodex.data.network.NetworkState
import com.stellaclaw.stellacodex.data.store.ConnectionProfileStore
import com.stellaclaw.stellacodex.data.store.connectionDataStore
import com.stellaclaw.stellacodex.domain.model.ChatMessage
import com.stellaclaw.stellacodex.domain.model.ConversationSummary
import com.stellaclaw.stellacodex.domain.model.MessageItem
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlin.math.absoluteValue

class AgentCompletionService : Service() {
    private val serviceJob = SupervisorJob()
    private val serviceScope = CoroutineScope(serviceJob + Dispatchers.Main.immediate)
    private val api = StellaclawApi()
    private lateinit var store: ConnectionProfileStore
    private var pollJob: Job? = null
    private var consecutiveFailures = 0
    private val watches = linkedMapOf<String, Watch>()

    override fun onCreate() {
        super.onCreate()
        store = ConnectionProfileStore(applicationContext.connectionDataStore)
        NetworkMonitor.start(applicationContext)
        ensureRunningChannel()
        watches.putAll(loadWatches())
        serviceScope.launch {
            NetworkMonitor.state.collect { state ->
                if (state == NetworkState.Available && watches.isNotEmpty()) {
                    log("network restored; poll immediately")
                    runCatching { pollOnce() }
                        .onFailure { error -> log("network-resume poll failed ${error::class.java.simpleName}: ${error.message.orEmpty()}") }
                }
            }
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ActionStop -> {
                val conversationId = intent.getStringExtra(ExtraConversationId).orEmpty()
                if (conversationId.isBlank()) {
                    watches.clear()
                } else {
                    watches.remove(conversationId)
                }
                saveWatches()
                if (watches.isEmpty()) stopSelf()
                return START_NOT_STICKY
            }
            ActionWatch -> {
                val conversationId = intent.getStringExtra(ExtraConversationId).orEmpty()
                val baselineIndex = intent.getIntExtra(ExtraBaselineIndex, -1)
                if (conversationId.isNotBlank()) {
                    watches[conversationId] = Watch(conversationId, baselineIndex)
                    saveWatches()
                    log("watch conversation=$conversationId baseline=$baselineIndex")
                }
            }
            else -> Unit
        }
        runCatching {
            startForeground(NotificationId, runningNotification("Checking agent conversations"))
        }.onFailure { error ->
            log("startForeground failed ${error::class.java.simpleName}: ${error.message.orEmpty()}")
            stopSelf()
            return START_NOT_STICKY
        }
        ensurePollLoop()
        return START_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        pollJob?.cancel()
        serviceJob.cancel()
        super.onDestroy()
    }

    private fun ensurePollLoop() {
        if (pollJob?.isActive == true) return
        pollJob = serviceScope.launch {
            while (isActive) {
                val success = runCatching { pollOnce() }
                    .onFailure { error -> log("poll loop failure ${error::class.java.simpleName}: ${error.message.orEmpty()}") }
                    .getOrDefault(false)
                consecutiveFailures = if (success) 0 else consecutiveFailures + 1
                delay(nextPollDelayMillis(success))
            }
        }
    }

    private suspend fun pollOnce(): Boolean {
        if (!NetworkMonitor.isAvailable()) {
            log("poll skipped while offline")
            updateRunningNotification("Offline · will resume when network returns")
            return false
        }
        val profile = store.profile.first()
        val summaries = when (val result = api.loadConversations(profile, limit = 200)) {
            is AppResult.Err -> {
                log("summary poll failed error=${result.error}")
                updateRunningNotification("Network unstable · retrying in ${nextPollDelayMillis(false) / 1000}s")
                return false
            }
            is AppResult.Ok -> result.value
        }
        addRunningConversations(summaries)
        if (watches.isEmpty()) {
            log("no running conversations; stop service")
            stopSelf()
            return true
        }
        updateRunningNotification()
        val runningById = summaries.associateBy { it.conversationId }
        val completed = mutableListOf<String>()
        watches.values.toList().forEach { watch ->
            val summary = runningById[watch.conversationId]
            if (summary == null) {
                val nextMissingCount = watch.missingCount + 1
                if (nextMissingCount >= 3) {
                    completed += watch.conversationId
                    log("drop missing conversation=${watch.conversationId} missing_count=$nextMissingCount")
                } else {
                    watches[watch.conversationId] = watch.copy(missingCount = nextMissingCount)
                    log("conversation temporarily missing conversation=${watch.conversationId} missing_count=$nextMissingCount")
                }
                return@forEach
            }
            if (watch.missingCount > 0) {
                watches[watch.conversationId] = watch.copy(missingCount = 0)
            }
            val latestAssistant = loadLatestFinalAssistant(watch)
            if (latestAssistant != null) {
                notifyCompletion(watch.conversationId, latestAssistant)
                completed += watch.conversationId
            } else if (!summary.running) {
                completed += watch.conversationId
                log("conversation stopped without final assistant conversation=${watch.conversationId}")
            } else {
                log("still running conversation=${watch.conversationId} baseline=${watch.baselineIndex}")
            }
        }
        completed.forEach { watches.remove(it) }
        if (completed.isNotEmpty()) saveWatches()
        if (watches.isEmpty()) stopSelf()
        return true
    }

    private fun nextPollDelayMillis(success: Boolean): Long = if (success) {
        PollDelayMillis
    } else {
        when (consecutiveFailures) {
            0 -> 5_000L
            1 -> 15_000L
            2 -> 30_000L
            else -> PollDelayMillis
        }
    }

    private fun addRunningConversations(summaries: List<ConversationSummary>) {
        summaries.filter { it.running }.forEach { summary ->
            if (watches.containsKey(summary.conversationId)) return@forEach
            val baseline = summary.lastMessageId?.toIntOrNull() ?: (summary.messageCount - 1).coerceAtLeast(-1)
            watches[summary.conversationId] = Watch(summary.conversationId, baseline)
            log("auto-watch running conversation=${summary.conversationId} baseline=$baseline")
        }
        saveWatches()
    }

    private suspend fun loadLatestFinalAssistant(watch: Watch): ChatMessage? =
        when (val result = api.loadLatestMessages(store.profile.first(), watch.conversationId, limit = 30)) {
            is AppResult.Err -> {
                log("message poll failed conversation=${watch.conversationId} error=${result.error}")
                null
            }
            is AppResult.Ok -> result.value.messages
                .filter { it.index > watch.baselineIndex }
                .lastOrNull { it.isFinalAssistantMessage() }
        }

    private fun notifyCompletion(conversationId: String, latestAssistant: ChatMessage) {
        val detail = latestAssistant.text.ifBlank { latestAssistant.preview }.take(160).ifBlank { null }
        val notified = AgentNotificationCenter.notifyAgentDone(
            context = applicationContext,
            conversationId = conversationId,
            title = "Agent finished",
            detail = detail,
            completionKey = "$conversationId:${latestAssistant.index}",
        )
        log("completion conversation=$conversationId index=${latestAssistant.index} notified=$notified")
    }

    private fun updateRunningNotification(textOverride: String? = null) {
        val count = watches.size
        val text = textOverride ?: if (count == 1) "Watching 1 agent conversation" else "Watching $count agent conversations"
        runCatching {
            getSystemService(NotificationManager::class.java).notify(NotificationId, runningNotification(text))
        }.onFailure { error ->
            log("update running notification failed ${error::class.java.simpleName}: ${error.message.orEmpty()}")
        }
    }

    private fun runningNotification(text: String) = NotificationCompat.Builder(this, RunningChannelId)
        .setSmallIcon(R.drawable.ic_launcher)
        .setContentTitle("Agent running")
        .setContentText(text)
        .setStyle(NotificationCompat.BigTextStyle().bigText(text))
        .setContentIntent(openAppIntent())
        .setOngoing(true)
        .setPriority(NotificationCompat.PRIORITY_LOW)
        .build()

    private fun openAppIntent(): PendingIntent {
        val intent = Intent(this, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP
        }
        return PendingIntent.getActivity(
            this,
            NotificationId.absoluteValue + 1,
            intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
    }

    private fun ensureRunningChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val manager = getSystemService(NotificationManager::class.java)
        if (manager.getNotificationChannel(RunningChannelId) != null) return
        manager.createNotificationChannel(
            NotificationChannel(
                RunningChannelId,
                "Agent running",
                NotificationManager.IMPORTANCE_LOW,
            ).apply {
                description = "Keeps agent completion polling active while agents are running"
            },
        )
    }

    private fun loadWatches(): Map<String, Watch> {
        val prefs = getSharedPreferences(PrefsName, Context.MODE_PRIVATE)
        return prefs.getStringSet(PrefsWatches, emptySet()).orEmpty()
            .mapNotNull { encoded ->
                val parts = encoded.split('|', limit = 2)
                val conversationId = parts.getOrNull(0)?.takeIf { it.isNotBlank() } ?: return@mapNotNull null
                val baseline = parts.getOrNull(1)?.toIntOrNull() ?: -1
                conversationId to Watch(conversationId, baseline)
            }
            .toMap()
    }

    private fun saveWatches() {
        val encoded = watches.values.map { "${it.conversationId}|${it.baselineIndex}" }.toSet()
        getSharedPreferences(PrefsName, Context.MODE_PRIVATE)
            .edit()
            .putStringSet(PrefsWatches, encoded)
            .apply()
    }

    private fun log(message: String) {
        AppLogStore.append(applicationContext, "agent-service", message)
    }

    private fun ChatMessage.isFinalAssistantMessage(): Boolean =
        role.equals("assistant", ignoreCase = true) &&
            !isRuntimeMetadataMessage() &&
            !isToolOnlyMessage() &&
            (text.isNotBlank() || preview.isNotBlank() || attachments.isNotEmpty())

    private fun ChatMessage.isRuntimeMetadataMessage(): Boolean {
        val body = text.ifBlank { preview }.trimStart()
        return body.startsWith("[Incoming User Metadata]") ||
            body.startsWith("[Incoming Assistant Metadata]") ||
            body.startsWith("[Incoming System Metadata]")
    }

    private fun ChatMessage.isToolOnlyMessage(): Boolean =
        text.isBlank() &&
            attachments.isEmpty() &&
            items.any { it is MessageItem.ToolCall || it is MessageItem.ToolResult }

    private data class Watch(
        val conversationId: String,
        val baselineIndex: Int,
        val missingCount: Int = 0,
    )

    companion object {
        private const val RunningChannelId = "agent_running"
        private const val NotificationId = 1201
        private const val ExtraConversationId = "conversation_id"
        private const val ExtraBaselineIndex = "baseline_index"
        private const val ActionWatch = "com.stellaclaw.stellacodex.action.WATCH_AGENT_CONVERSATION"
        private const val ActionStop = "com.stellaclaw.stellacodex.action.STOP_AGENT_COMPLETION_SERVICE"
        private const val PollDelayMillis = 60_000L
        private const val PrefsName = "agent_completion_service"
        private const val PrefsWatches = "watches"

        fun start(context: Context) {
            runCatching {
                ContextCompat.startForegroundService(context, Intent(context, AgentCompletionService::class.java))
            }.onFailure { error ->
                AppLogStore.append(context, "agent-service", "start service failed ${error::class.java.simpleName}: ${error.message.orEmpty()}")
            }
        }

        fun watch(context: Context, conversationId: String, baselineIndex: Int) {
            if (conversationId.isBlank()) return
            val intent = Intent(context, AgentCompletionService::class.java).apply {
                action = ActionWatch
                putExtra(ExtraConversationId, conversationId)
                putExtra(ExtraBaselineIndex, baselineIndex)
            }
            runCatching {
                ContextCompat.startForegroundService(context, intent)
            }.onFailure { error ->
                AppLogStore.append(context, "agent-service", "watch start failed conversation=$conversationId ${error::class.java.simpleName}: ${error.message.orEmpty()}")
            }
        }

        fun stop(context: Context, conversationId: String) {
            val intent = Intent(context, AgentCompletionService::class.java).apply {
                action = ActionStop
                putExtra(ExtraConversationId, conversationId)
            }
            runCatching {
                context.startService(intent)
            }.onFailure { error ->
                AppLogStore.append(context, "agent-service", "stop service failed conversation=$conversationId ${error::class.java.simpleName}: ${error.message.orEmpty()}")
            }
        }
    }
}

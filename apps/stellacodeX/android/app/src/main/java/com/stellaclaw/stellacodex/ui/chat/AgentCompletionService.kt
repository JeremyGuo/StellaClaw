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
import com.stellaclaw.stellacodex.data.store.ConnectionProfileStore
import com.stellaclaw.stellacodex.data.store.connectionDataStore
import com.stellaclaw.stellacodex.domain.model.ChatMessage
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
    private var activeConversationId: String = ""
    private var activeBaselineIndex: Int = -1

    override fun onCreate() {
        super.onCreate()
        store = ConnectionProfileStore(applicationContext.connectionDataStore)
        ensureRunningChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ActionStop) {
            stopSelf()
            return START_NOT_STICKY
        }
        val conversationId = intent?.getStringExtra(ExtraConversationId)?.takeIf { it.isNotBlank() }
            ?: return START_NOT_STICKY
        val baselineIndex = intent.getIntExtra(ExtraBaselineIndex, -1)
        activeConversationId = conversationId
        activeBaselineIndex = baselineIndex
        startForeground(NotificationId, runningNotification(conversationId))
        log("service start conversation=$conversationId baseline=$baselineIndex")
        pollJob?.cancel()
        pollJob = serviceScope.launch {
            pollUntilComplete(conversationId, baselineIndex)
        }
        return START_REDELIVER_INTENT
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        pollJob?.cancel()
        serviceJob.cancel()
        super.onDestroy()
    }

    private suspend fun pollUntilComplete(conversationId: String, baselineIndex: Int) {
        repeat(MaxPolls) { attempt ->
            if (!serviceScope.coroutineContext.isActive || activeConversationId != conversationId) return
            if (attempt > 0) delay(PollDelayMillis)
            log("service poll conversation=$conversationId baseline=$baselineIndex attempt=$attempt")
            val profile = store.profile.first()
            when (val result = api.loadLatestMessages(profile, conversationId, limit = 30)) {
                is AppResult.Err -> log("service poll failed conversation=$conversationId error=${result.error}")
                is AppResult.Ok -> {
                    val latestAssistant = result.value.messages
                        .filter { it.index > baselineIndex }
                        .lastOrNull { it.isFinalAssistantMessage() }
                    if (latestAssistant != null) {
                        val detail = latestAssistant.text.ifBlank { latestAssistant.preview }.take(160).ifBlank { null }
                        val notified = AgentNotificationCenter.notifyAgentDone(
                            context = applicationContext,
                            conversationId = conversationId,
                            title = "Agent finished",
                            detail = detail,
                            completionKey = "$conversationId:${latestAssistant.index}",
                        )
                        log("service completion conversation=$conversationId index=${latestAssistant.index} notified=$notified")
                        stopSelf()
                        return
                    }
                    log("service no completion conversation=$conversationId latestTotal=${result.value.total}")
                }
            }
        }
        log("service give up conversation=$conversationId baseline=$baselineIndex")
        stopSelf()
    }

    private fun runningNotification(conversationId: String) = NotificationCompat.Builder(this, RunningChannelId)
        .setSmallIcon(R.drawable.ic_launcher)
        .setContentTitle("Agent running")
        .setContentText("Waiting for agent completion")
        .setStyle(NotificationCompat.BigTextStyle().bigText("Watching $conversationId for the final assistant reply"))
        .setContentIntent(openAppIntent(conversationId))
        .setOngoing(true)
        .setPriority(NotificationCompat.PRIORITY_LOW)
        .build()

    private fun openAppIntent(conversationId: String): PendingIntent {
        val intent = Intent(this, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP
        }
        return PendingIntent.getActivity(
            this,
            conversationId.hashCode().absoluteValue + 1,
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
                description = "Keeps agent completion polling active while an agent is running"
            },
        )
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

    companion object {
        private const val RunningChannelId = "agent_running"
        private const val NotificationId = 1201
        private const val ExtraConversationId = "conversation_id"
        private const val ExtraBaselineIndex = "baseline_index"
        private const val ActionStop = "com.stellaclaw.stellacodex.action.STOP_AGENT_COMPLETION_SERVICE"
        private const val PollDelayMillis = 15_000L
        private const val MaxPolls = 120

        fun start(context: Context, conversationId: String, baselineIndex: Int) {
            if (conversationId.isBlank()) return
            AgentCompletionPollWorker.cancel(context, conversationId)
            val intent = Intent(context, AgentCompletionService::class.java).apply {
                putExtra(ExtraConversationId, conversationId)
                putExtra(ExtraBaselineIndex, baselineIndex)
            }
            ContextCompat.startForegroundService(context, intent)
        }

        fun stop(context: Context, conversationId: String) {
            AgentCompletionPollWorker.cancel(context, conversationId)
            val intent = Intent(context, AgentCompletionService::class.java).apply {
                action = ActionStop
            }
            context.startService(intent)
        }
    }
}

package com.stellaclaw.stellacodex.ui.chat

import android.content.Context
import androidx.work.BackoffPolicy
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import androidx.work.workDataOf
import com.stellaclaw.stellacodex.core.result.AppResult
import com.stellaclaw.stellacodex.data.api.StellaclawApi
import com.stellaclaw.stellacodex.data.log.AppLogStore
import com.stellaclaw.stellacodex.data.store.ConnectionProfileStore
import com.stellaclaw.stellacodex.data.store.connectionDataStore
import com.stellaclaw.stellacodex.domain.model.ChatMessage
import com.stellaclaw.stellacodex.domain.model.MessageItem
import kotlinx.coroutines.flow.first
import java.util.concurrent.TimeUnit

class AgentCompletionPollWorker(
    appContext: Context,
    params: WorkerParameters,
) : CoroutineWorker(appContext, params) {
    private val store = ConnectionProfileStore(appContext.connectionDataStore)
    private val api = StellaclawApi()

    override suspend fun doWork(): Result {
        val conversationId = inputData.getString(KeyConversationId)?.takeIf { it.isNotBlank() }
            ?: return Result.failure()
        val baselineIndex = inputData.getInt(KeyBaselineIndex, -1)
        val remainingAttempts = MaxAttempts - runAttemptCount
        if (remainingAttempts <= 0) {
            log("give up conversation=$conversationId baseline=$baselineIndex")
            return Result.success()
        }

        log("poll conversation=$conversationId baseline=$baselineIndex attempt=$runAttemptCount")
        val profile = store.profile.first()
        return when (val result = api.loadLatestMessages(profile, conversationId, limit = 30)) {
            is AppResult.Err -> {
                log("poll failed conversation=$conversationId error=${result.error}")
                Result.retry()
            }
            is AppResult.Ok -> {
                val latestAssistant = result.value.messages
                    .filter { it.index > baselineIndex }
                    .lastOrNull { message -> message.isFinalAssistantMessage() }
                if (latestAssistant != null) {
                    val detail = latestAssistant.text.ifBlank { latestAssistant.preview }.take(160).ifBlank { null }
                    val notified = AgentNotificationCenter.notifyAgentDone(
                        context = applicationContext,
                        conversationId = conversationId,
                        title = "Agent finished",
                        detail = detail,
                        completionKey = "$conversationId:${latestAssistant.index}",
                    )
                    log("completion found conversation=$conversationId index=${latestAssistant.index} notified=$notified")
                    if (notified) {
                        Result.success()
                    } else {
                        Result.retry()
                    }
                } else {
                    log("no completion yet conversation=$conversationId latestTotal=${result.value.total} remainingAttempts=$remainingAttempts")
                    Result.retry()
                }
            }
        }
    }

    private fun log(message: String) {
        AppLogStore.append(applicationContext, "agent-poll", message)
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
        private const val KeyConversationId = "conversation_id"
        private const val KeyBaselineIndex = "baseline_index"
        private const val MaxAttempts = 30
        private const val InitialDelaySeconds = 15L

        fun schedule(context: Context, conversationId: String, baselineIndex: Int) {
            if (conversationId.isBlank()) return
            enqueue(
                context = context,
                conversationId = conversationId,
                baselineIndex = baselineIndex,
                initialDelaySeconds = InitialDelaySeconds,
                replace = true,
            )
        }

        fun cancel(context: Context, conversationId: String) {
            if (conversationId.isBlank()) return
            WorkManager.getInstance(context).cancelUniqueWork(uniqueName(conversationId))
        }

        private fun enqueue(
            context: Context,
            conversationId: String,
            baselineIndex: Int,
            initialDelaySeconds: Long,
            replace: Boolean,
        ) {
            val request = OneTimeWorkRequestBuilder<AgentCompletionPollWorker>()
                .setConstraints(
                    Constraints.Builder()
                        .setRequiredNetworkType(NetworkType.CONNECTED)
                        .build(),
                )
                .setInitialDelay(initialDelaySeconds, TimeUnit.SECONDS)
                .setBackoffCriteria(BackoffPolicy.LINEAR, 1, TimeUnit.MINUTES)
                .setInputData(
                    workDataOf(
                        KeyConversationId to conversationId,
                        KeyBaselineIndex to baselineIndex,
                    ),
                )
                .build()
            WorkManager.getInstance(context).enqueueUniqueWork(
                uniqueName(conversationId),
                if (replace) ExistingWorkPolicy.REPLACE else ExistingWorkPolicy.KEEP,
                request,
            )
        }

        private fun uniqueName(conversationId: String): String = "agent-completion-$conversationId"
    }
}

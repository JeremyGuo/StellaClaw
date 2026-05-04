package com.stellaclaw.stellacodex.ui.chat

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.RingtoneManager
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import com.stellaclaw.stellacodex.MainActivity
import com.stellaclaw.stellacodex.R
import kotlin.math.absoluteValue

object AgentNotificationCenter {
    private const val ChannelId = "agent_done_heads_up"

    fun canNotify(context: Context): Boolean =
        Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED

    fun dismissConversation(context: Context, conversationId: String) {
        NotificationManagerCompat.from(context).cancel(notificationId(conversationId))
    }

    fun notifyAgentDone(
        context: Context,
        conversationId: String,
        title: String,
        detail: String?,
        completionKey: String? = null,
    ): Boolean {
        if (!canNotify(context)) return false
        if (completionKey != null && isAlreadyNotified(context, completionKey)) return true
        ensureChannel(context)
        val intent = Intent(context, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP
            putExtra(MainActivity.EXTRA_CONVERSATION_ID, conversationId)
        }
        val pendingIntent = PendingIntent.getActivity(
            context,
            notificationId(conversationId),
            intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = NotificationCompat.Builder(context, ChannelId)
            .setSmallIcon(R.drawable.ic_launcher)
            .setContentTitle(title)
            .setContentText(detail ?: "Agent finished replying")
            .setStyle(NotificationCompat.BigTextStyle().bigText(detail ?: "Agent finished replying"))
            .setContentIntent(pendingIntent)
            .setAutoCancel(true)
            .setCategory(NotificationCompat.CATEGORY_MESSAGE)
            .setVisibility(NotificationCompat.VISIBILITY_PUBLIC)
            .setDefaults(NotificationCompat.DEFAULT_ALL)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .build()
        return try {
            NotificationManagerCompat.from(context).notify(notificationId(conversationId), notification)
            completionKey?.let { markNotified(context, it) }
            true
        } catch (_: SecurityException) {
            false
        }
    }

    private fun notificationId(conversationId: String): Int = conversationId.hashCode().absoluteValue

    private fun isAlreadyNotified(context: Context, completionKey: String): Boolean {
        val prefs = context.getSharedPreferences("agent_notifications", Context.MODE_PRIVATE)
        return prefs.getString("last_completion_key", null) == completionKey
    }

    private fun markNotified(context: Context, completionKey: String) {
        context.getSharedPreferences("agent_notifications", Context.MODE_PRIVATE)
            .edit()
            .putString("last_completion_key", completionKey)
            .apply()
    }

    private fun ensureChannel(context: Context) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val manager = context.getSystemService(NotificationManager::class.java)
        if (manager.getNotificationChannel(ChannelId) != null) return
        manager.createNotificationChannel(
            NotificationChannel(
                ChannelId,
                "Agent completion alerts",
                NotificationManager.IMPORTANCE_HIGH,
            ).apply {
                description = "Shows a heads-up alert when an agent finishes replying"
                enableVibration(true)
                setSound(
                    RingtoneManager.getDefaultUri(RingtoneManager.TYPE_NOTIFICATION),
                    android.media.AudioAttributes.Builder()
                        .setUsage(android.media.AudioAttributes.USAGE_NOTIFICATION)
                        .build(),
                )
            },
        )
    }
}

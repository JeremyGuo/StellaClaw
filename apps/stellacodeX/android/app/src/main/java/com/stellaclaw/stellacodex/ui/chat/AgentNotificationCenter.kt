package com.stellaclaw.stellacodex.ui.chat

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import com.stellaclaw.stellacodex.MainActivity
import com.stellaclaw.stellacodex.R
import kotlin.math.absoluteValue

object AgentNotificationCenter {
    private const val ChannelId = "agent_done"

    fun canNotify(context: Context): Boolean =
        Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED

    fun notifyAgentDone(context: Context, conversationId: String, title: String, detail: String?) {
        if (!canNotify(context)) return
        ensureChannel(context)
        val intent = Intent(context, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP
        }
        val pendingIntent = PendingIntent.getActivity(
            context,
            conversationId.hashCode().absoluteValue,
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
            .setPriority(NotificationCompat.PRIORITY_DEFAULT)
            .build()
        NotificationManagerCompat.from(context).notify(conversationId.hashCode().absoluteValue, notification)
    }

    private fun ensureChannel(context: Context) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val manager = context.getSystemService(NotificationManager::class.java)
        if (manager.getNotificationChannel(ChannelId) != null) return
        manager.createNotificationChannel(
            NotificationChannel(
                ChannelId,
                "Agent completion",
                NotificationManager.IMPORTANCE_DEFAULT,
            ).apply {
                description = "Notifies when an agent finishes replying"
            },
        )
    }
}

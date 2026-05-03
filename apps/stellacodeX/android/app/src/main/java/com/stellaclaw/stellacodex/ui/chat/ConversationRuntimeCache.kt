package com.stellaclaw.stellacodex.ui.chat

import com.stellaclaw.stellacodex.domain.model.ChatMessage
import com.stellaclaw.stellacodex.domain.model.ConnectionProfile

/**
 * Process-lifetime chat history cache. It is intentionally not persisted: Android drops it when the
 * app process exits, but conversation switches within the same run can reuse already loaded pages.
 */
object ConversationRuntimeCache {
    private const val MaxEntries = 12
    private val lock = Any()
    private val entries = object : LinkedHashMap<String, CachedChatSnapshot>(16, 0.75f, true) {
        override fun removeEldestEntry(eldest: MutableMap.MutableEntry<String, CachedChatSnapshot>?): Boolean =
            size > MaxEntries
    }

    fun get(profile: ConnectionProfile, conversationId: String): CachedChatSnapshot? = synchronized(lock) {
        entries[cacheKey(profile, conversationId)]
    }

    fun put(profile: ConnectionProfile, conversationId: String, snapshot: CachedChatSnapshot) {
        if (conversationId.isBlank()) return
        if (snapshot.messages.isEmpty() && snapshot.totalMessages == 0) return
        synchronized(lock) {
            entries[cacheKey(profile, conversationId)] = snapshot
        }
    }

    private fun cacheKey(profile: ConnectionProfile, conversationId: String): String = listOf(
        profile.connectionMode.wireName,
        profile.baseUrl.trim(),
        profile.effectiveTargetUrl.trim(),
        profile.sshHost.trim(),
        profile.sshPort.trim(),
        profile.sshUser.trim(),
        profile.token.hashCode().toString(),
        conversationId,
    ).joinToString("|")
}

data class CachedChatSnapshot(
    val displayName: String,
    val messages: List<ChatMessage>,
    val loadedOffset: Int,
    val totalMessages: Int,
)

package com.stellaclaw.stellacodex.data.log

import android.content.Context
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import java.time.Instant

object AppLogStore {
    private const val MaxLines = 600
    private const val LogFileName = "stellacodex-debug.log"

    private val lock = Any()
    private val lines = ArrayDeque<String>()
    private val mutableText = MutableStateFlow("")
    val text: StateFlow<String> = mutableText.asStateFlow()

    fun initialize(context: Context) {
        synchronized(lock) {
            if (lines.isNotEmpty()) return
            val file = context.applicationContext.filesDir.resolve(LogFileName)
            if (file.exists()) {
                file.readLines()
                    .takeLast(MaxLines)
                    .forEach(lines::addLast)
                publishLocked(context.applicationContext)
            }
        }
    }

    fun append(context: Context, tag: String, message: String) {
        val appContext = context.applicationContext
        synchronized(lock) {
            val line = "${Instant.now()} [$tag] ${message.replace('\n', ' ')}"
            lines.addLast(line)
            while (lines.size > MaxLines) {
                lines.removeFirst()
            }
            publishLocked(appContext)
        }
    }

    fun clear(context: Context) {
        val appContext = context.applicationContext
        synchronized(lock) {
            lines.clear()
            publishLocked(appContext)
        }
    }

    private fun publishLocked(context: Context) {
        val value = lines.joinToString("\n")
        mutableText.value = value
        runCatching {
            context.filesDir.resolve(LogFileName).writeText(value)
        }
    }
}

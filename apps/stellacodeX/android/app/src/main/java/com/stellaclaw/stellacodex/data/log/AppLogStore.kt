package com.stellaclaw.stellacodex.data.log

import android.content.Context
import android.util.Log
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import java.time.Instant

object AppLogStore {
    private const val MaxLines = 600
    private const val LogFileName = "stellacodex-debug.log"
    private const val LogcatTagPrefix = "StellacodeX"

    private val lock = Any()
    private val lines = ArrayDeque<String>()
    private val mutableText = MutableStateFlow("")
    val text: StateFlow<String> = mutableText.asStateFlow()

    @Volatile
    private var crashHandlerInstalled = false

    fun initialize(context: Context) {
        val appContext = context.applicationContext
        synchronized(lock) {
            if (lines.isNotEmpty()) return
            runCatching {
                val file = appContext.filesDir.resolve(LogFileName)
                if (file.exists()) {
                    file.readLines()
                        .takeLast(MaxLines)
                        .forEach(lines::addLast)
                    publishLocked(appContext)
                }
            }.onFailure { error ->
                Log.w(LogcatTagPrefix, "Failed to initialize app log store", error)
            }
        }
    }

    fun installCrashHandler(context: Context) {
        val appContext = context.applicationContext
        initialize(appContext)
        if (crashHandlerInstalled) return
        synchronized(lock) {
            if (crashHandlerInstalled) return
            val previous = Thread.getDefaultUncaughtExceptionHandler()
            Thread.setDefaultUncaughtExceptionHandler { thread, throwable ->
                append(appContext, "crash", "uncaught thread=${thread.name} ${throwable.stackTraceText()}")
                previous?.uncaughtException(thread, throwable)
            }
            crashHandlerInstalled = true
        }
    }

    fun append(context: Context, tag: String, message: String) {
        val appContext = context.applicationContext
        val safeMessage = message.replace('\n', ' ')
        Log.d("$LogcatTagPrefix/$tag", safeMessage)
        synchronized(lock) {
            val line = "${Instant.now()} [$tag] $safeMessage"
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
        }.onFailure { error ->
            Log.w(LogcatTagPrefix, "Failed to write app log file", error)
        }
    }

    private fun Throwable.stackTraceText(): String = buildString {
        append(this@stackTraceText::class.java.name)
        message?.let { append(": ").append(it) }
        stackTrace.take(24).forEach { frame -> append(" | at ").append(frame.toString()) }
        cause?.let { cause ->
            append(" | caused by ").append(cause::class.java.name)
            cause.message?.let { append(": ").append(it) }
            cause.stackTrace.take(12).forEach { frame -> append(" | at ").append(frame.toString()) }
        }
    }
}

package com.stellaclaw.stellacodex.ui.settings

import android.app.Application
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.Settings
import androidx.core.content.FileProvider
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.stellaclaw.stellacodex.data.log.AppLogStore
import com.stellaclaw.stellacodex.data.store.ConnectionProfileStore
import com.stellaclaw.stellacodex.data.store.connectionDataStore
import com.stellaclaw.stellacodex.data.update.ApkReleaseChannel
import com.stellaclaw.stellacodex.data.update.ApkUpdateDownloader
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

class SettingsViewModel(application: Application) : AndroidViewModel(application) {
    private val store = ConnectionProfileStore(application.connectionDataStore)
    private val downloader = ApkUpdateDownloader()
    private val mutableState = MutableStateFlow(SettingsUiState())
    val state: StateFlow<SettingsUiState> = mutableState.asStateFlow()

    fun installTest() = downloadAndInstall(ApkReleaseChannel.Test)

    fun installStable() = downloadAndInstall(ApkReleaseChannel.Stable)

    private fun downloadAndInstall(channel: ApkReleaseChannel) {
        if (state.value.isDownloading) return
        mutableState.update {
            it.copy(
                isDownloading = true,
                status = "Preparing ${channel.label} update...",
                error = null,
                downloadProgress = null,
                downloadedBytes = null,
                totalBytes = null,
            )
        }
        viewModelScope.launch {
            val app = getApplication<Application>()
            try {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O && !app.packageManager.canRequestPackageInstalls()) {
                    val intent = Intent(
                        Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
                        Uri.parse("package:${app.packageName}"),
                    ).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    app.startActivity(intent)
                    mutableState.update {
                        it.copy(
                            isDownloading = false,
                            status = "Allow installs from StellacodeX, then tap ${channel.label} again.",
                        )
                    }
                    return@launch
                }
                val profile = store.profile.first()
                val apk = withContext(Dispatchers.IO) {
                    downloader.download(app, profile, channel) { progress ->
                        AppLogStore.append(app, "update", progress.message)
                        mutableState.update {
                            it.copy(
                                status = progress.message,
                                downloadProgress = progress.fraction,
                                downloadedBytes = progress.bytesDownloaded,
                                totalBytes = progress.totalBytes?.takeIf { total -> total > 0L },
                            )
                        }
                    }
                }
                AppLogStore.append(app, "update", "Launching installer for ${apk.name}")
                val uri = FileProvider.getUriForFile(app, "${app.packageName}.fileprovider", apk)
                val intent = Intent(Intent.ACTION_VIEW)
                    .setDataAndType(uri, "application/vnd.android.package-archive")
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                app.startActivity(intent)
                mutableState.update {
                    it.copy(
                        isDownloading = false,
                        status = "Installer opened for ${channel.label} APK.",
                        downloadProgress = 1f,
                    )
                }
            } catch (error: Exception) {
                val message = error.message.orEmpty().ifBlank { error::class.simpleName ?: "update failed" }
                AppLogStore.append(app, "update", "${channel.label} update failed: $message")
                mutableState.update {
                    it.copy(
                        isDownloading = false,
                        error = message,
                        status = "Update failed",
                        downloadProgress = null,
                    )
                }
            }
        }
    }
}

data class SettingsUiState(
    val isDownloading: Boolean = false,
    val status: String = "",
    val error: String? = null,
    val downloadProgress: Float? = null,
    val downloadedBytes: Long? = null,
    val totalBytes: Long? = null,
)

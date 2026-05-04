package com.stellaclaw.stellacodex.data.update

import android.content.Context
import com.jcraft.jsch.ChannelSftp
import com.jcraft.jsch.JSch
import com.jcraft.jsch.JSchException
import com.jcraft.jsch.Session
import com.jcraft.jsch.SftpProgressMonitor
import com.stellaclaw.stellacodex.domain.model.ConnectionProfile
import java.io.File
import java.util.Properties
import java.util.concurrent.TimeUnit

class ApkUpdateDownloader {
    fun download(
        context: Context,
        profile: ConnectionProfile,
        channel: ApkReleaseChannel,
        onProgress: (ApkDownloadProgress) -> Unit,
    ): File {
        require(profile.sshHost.isNotBlank() && profile.sshUser.isNotBlank()) {
            "SSH host and user are required for app update downloads"
        }
        val targetFile = apkCacheDir(context)
            .also { it.mkdirs() }
            .resolve("stellacodex-${channel.wireName}.apk")
        if (targetFile.exists()) {
            targetFile.delete()
            onProgress(ApkDownloadProgress(message = "Removed old ${targetFile.name} before download"))
        }
        val session = openSession(profile, onProgress)
        try {
            val sftp = session.openChannel("sftp") as ChannelSftp
            sftp.connect(10_000)
            try {
                val totalBytes = runCatching { sftp.lstat(channel.remotePath).size }.getOrDefault(-1L)
                onProgress(
                    ApkDownloadProgress(
                        message = "Downloading ${channel.label} APK from ${channel.remotePath}",
                        bytesDownloaded = 0L,
                        totalBytes = totalBytes,
                    ),
                )
                targetFile.outputStream().use { output ->
                    sftp.get(
                        channel.remotePath,
                        output,
                        ApkSftpProgressMonitor(totalBytes, onProgress),
                    )
                }
                if (targetFile.length() <= 0L) {
                    error("Downloaded APK is empty")
                }
                onProgress(
                    ApkDownloadProgress(
                        message = "Downloaded ${formatBytes(targetFile.length())} to ${targetFile.name}",
                        bytesDownloaded = targetFile.length(),
                        totalBytes = totalBytes,
                    ),
                )
                return targetFile
            } finally {
                sftp.disconnect()
            }
        } finally {
            session.disconnect()
        }
    }

    private fun openSession(profile: ConnectionProfile, onProgress: (ApkDownloadProgress) -> Unit): Session {
        val jsch = JSch()
        val privateKey = normalizePrivateKey(profile.sshPrivateKey)
        if (privateKey.isNotBlank()) {
            try {
                jsch.addIdentity(
                    "stellacodex-update-key",
                    privateKey.toByteArray(),
                    null,
                    profile.sshPassphrase.takeIf { it.isNotBlank() }?.toByteArray(),
                )
            } catch (error: JSchException) {
                throw IllegalStateException("Failed to parse SSH private key: ${error.message.orEmpty()}", error)
            }
        }
        val port = profile.sshPort.trim().toIntOrNull() ?: 22
        val session = jsch.getSession(profile.sshUser.trim(), profile.sshHost.trim(), port)
        profile.sshPassword.takeIf { it.isNotBlank() }?.let(session::setPassword)
        session.setConfig(
            Properties().apply {
                put("StrictHostKeyChecking", "no")
                put("PreferredAuthentications", "publickey,password,keyboard-interactive")
                put("signature.ed25519", "com.jcraft.jsch.bc.SignatureEd25519")
                put("signature.ed448", "com.jcraft.jsch.bc.SignatureEd448")
                put(
                    "PubkeyAcceptedAlgorithms",
                    "ssh-ed25519,ecdsa-sha2-nistp256,ecdsa-sha2-nistp384,ecdsa-sha2-nistp521,rsa-sha2-512,rsa-sha2-256,ssh-rsa",
                )
                put(
                    "server_host_key",
                    "ssh-ed25519,ecdsa-sha2-nistp256,ecdsa-sha2-nistp384,ecdsa-sha2-nistp521,rsa-sha2-512,rsa-sha2-256,ssh-rsa",
                )
            },
        )
        session.serverAliveInterval = 15_000
        session.serverAliveCountMax = 3
        onProgress(ApkDownloadProgress(message = "Connecting SSH ${profile.sshUser}@${profile.sshHost}:$port"))
        session.connect(10_000)
        return session
    }
    companion object {
        private val MaxCacheAgeMillis = TimeUnit.DAYS.toMillis(7)

        fun cleanupOldApks(context: Context, onCleanup: (String) -> Unit = {}) {
            val now = System.currentTimeMillis()
            val dir = apkCacheDir(context)
            if (!dir.exists()) return
            dir.listFiles { file -> file.isFile && file.extension.equals("apk", ignoreCase = true) }
                ?.forEach { file ->
                    if (now - file.lastModified() > MaxCacheAgeMillis) {
                        val name = file.name
                        if (file.delete()) {
                            onCleanup("Removed old cached APK $name")
                        }
                    }
                }
        }

        private fun apkCacheDir(context: Context): File = context.cacheDir.resolve("apk")
    }
}

data class ApkDownloadProgress(
    val message: String,
    val bytesDownloaded: Long? = null,
    val totalBytes: Long? = null,
) {
    val fraction: Float? = if (bytesDownloaded != null && totalBytes != null && totalBytes > 0L) {
        (bytesDownloaded.toDouble() / totalBytes.toDouble()).coerceIn(0.0, 1.0).toFloat()
    } else {
        null
    }
}

enum class ApkReleaseChannel(
    val wireName: String,
    val label: String,
    val remotePath: String,
) {
    Test(
        wireName = "test",
        label = "test",
        remotePath = "/home/liuhao/ClawParty/apps/stellacodeX/android/dist/stellacodex-android-test.apk",
    ),
    Stable(
        wireName = "stable",
        label = "stable",
        remotePath = "/home/liuhao/ClawParty/apps/stellacodeX/android/dist/stellacodex-android-stable.apk",
    ),
}

private fun normalizePrivateKey(value: String): String = value
    .trim()
    .replace("\r\n", "\n")
    .replace("\r", "\n")
    .replace("\\n", "\n")

private class ApkSftpProgressMonitor(
    private val totalBytes: Long,
    private val onProgress: (ApkDownloadProgress) -> Unit,
) : SftpProgressMonitor {
    private var downloaded = 0L
    private var lastReportAt = 0L

    override fun init(op: Int, src: String?, dest: String?, max: Long) {
        downloaded = 0L
        report(force = true)
    }

    override fun count(count: Long): Boolean {
        downloaded += count
        report(force = false)
        return true
    }

    override fun end() {
        report(force = true)
    }

    private fun report(force: Boolean) {
        val now = System.currentTimeMillis()
        if (!force && now - lastReportAt < 300L) return
        lastReportAt = now
        val percent = if (totalBytes > 0L) {
            " (${((downloaded.toDouble() / totalBytes.toDouble()) * 100).coerceIn(0.0, 100.0).toInt()}%)"
        } else {
            ""
        }
        val totalText = if (totalBytes > 0L) " / ${formatBytes(totalBytes)}" else ""
        onProgress(
            ApkDownloadProgress(
                message = "Downloading ${formatBytes(downloaded)}$totalText$percent",
                bytesDownloaded = downloaded,
                totalBytes = totalBytes,
            ),
        )
    }
}

private fun formatBytes(bytes: Long): String {
    if (bytes < 1024L) return "$bytes B"
    val kib = bytes / 1024.0
    if (kib < 1024.0) return "%.1f KiB".format(kib)
    val mib = kib / 1024.0
    if (mib < 1024.0) return "%.1f MiB".format(mib)
    return "%.1f GiB".format(mib / 1024.0)
}

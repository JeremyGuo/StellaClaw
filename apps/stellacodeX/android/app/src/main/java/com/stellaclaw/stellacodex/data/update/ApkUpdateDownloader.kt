package com.stellaclaw.stellacodex.data.update

import android.content.Context
import android.content.pm.PackageInfo
import android.os.Build
import com.jcraft.jsch.ChannelExec
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
        val appContext = context.applicationContext
        val targetFile = apkCacheDir(appContext)
            .also { it.mkdirs() }
            .resolve("stellacodex-${channel.wireName}.apk")
        val session = openSession(profile, onProgress)
        try {
            val remoteVersion = readRemoteApkVersion(session, channel.remotePath, onProgress)
            val cachedVersion = readLocalApkVersion(appContext, targetFile, onProgress)
            if (targetFile.exists() && targetFile.length() > 0L && remoteVersion != null && cachedVersion != null) {
                if (cachedVersion.samePackageVersion(remoteVersion)) {
                    onProgress(
                        ApkDownloadProgress(
                            message = "Cached ${targetFile.name} already matches ${channel.label} ${remoteVersion.versionLabel}; opening installer",
                            bytesDownloaded = targetFile.length(),
                            totalBytes = targetFile.length(),
                        ),
                    )
                    return targetFile
                }
                onProgress(
                    ApkDownloadProgress(
                        message = "Cached ${cachedVersion.versionLabel} differs from remote ${remoteVersion.versionLabel}; downloading update",
                    ),
                )
            } else if (targetFile.exists() && remoteVersion == null) {
                onProgress(ApkDownloadProgress(message = "Remote APK version unavailable; downloading fresh ${channel.label} APK"))
            }

            val sftp = session.openChannel("sftp") as ChannelSftp
            sftp.connect(10_000)
            try {
                val totalBytes = runCatching { sftp.lstat(channel.remotePath).size }.getOrDefault(-1L)
                if (targetFile.exists()) {
                    targetFile.delete()
                    onProgress(ApkDownloadProgress(message = "Removed old ${targetFile.name} before download"))
                }
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
                val downloadedVersion = readLocalApkVersion(appContext, targetFile, onProgress)
                onProgress(
                    ApkDownloadProgress(
                        message = "Downloaded ${downloadedVersion?.versionLabel ?: formatBytes(targetFile.length())} to ${targetFile.name}",
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

    private fun readRemoteApkVersion(
        session: Session,
        remotePath: String,
        onProgress: (ApkDownloadProgress) -> Unit,
    ): ApkVersionInfo? {
        val quotedPath = shellSingleQuote(remotePath)
        val command = "AAPT=\$(command -v aapt 2>/dev/null || command -v /usr/lib/android-sdk/build-tools/35.0.0/aapt 2>/dev/null || true); " +
            "if [ -n \"\$AAPT\" ]; then \"\$AAPT\" dump badging $quotedPath 2>/dev/null | sed -n '1p'; fi"
        val output = runCatching { execText(session, command, timeoutMillis = 15_000L) }
            .onFailure { error -> onProgress(ApkDownloadProgress(message = "Remote APK version check failed: ${error.message.orEmpty()}")) }
            .getOrDefault("")
            .trim()
        val version = parseAaptPackageLine(output)
        if (version != null) {
            onProgress(ApkDownloadProgress(message = "Remote ${version.versionLabel}"))
        } else {
            onProgress(ApkDownloadProgress(message = "Remote APK version metadata unavailable"))
        }
        return version
    }

    private fun execText(session: Session, command: String, timeoutMillis: Long): String {
        val channel = session.openChannel("exec") as ChannelExec
        channel.setCommand(command)
        channel.setInputStream(null)
        val stdout = channel.inputStream
        val stderr = channel.errStream
        channel.connect(5_000)
        val deadline = System.currentTimeMillis() + timeoutMillis
        val output = ByteArray(8192)
        val errorOutput = ByteArray(4096)
        val builder = StringBuilder()
        val errorBuilder = StringBuilder()
        try {
            while (!channel.isClosed && System.currentTimeMillis() < deadline) {
                while (stdout.available() > 0) {
                    val read = stdout.read(output, 0, output.size)
                    if (read > 0) builder.append(String(output, 0, read))
                }
                while (stderr.available() > 0) {
                    val read = stderr.read(errorOutput, 0, errorOutput.size)
                    if (read > 0) errorBuilder.append(String(errorOutput, 0, read))
                }
                Thread.sleep(50L)
            }
            while (stdout.available() > 0) {
                val read = stdout.read(output, 0, output.size)
                if (read > 0) builder.append(String(output, 0, read))
            }
            if (!channel.isClosed) error("Remote command timed out")
            if (channel.exitStatus != 0 && builder.isBlank()) {
                error(errorBuilder.toString().trim().ifBlank { "Remote command failed with exit ${channel.exitStatus}" })
            }
            return builder.toString()
        } finally {
            channel.disconnect()
        }
    }

    private fun readLocalApkVersion(
        context: Context,
        file: File,
        onProgress: (ApkDownloadProgress) -> Unit,
    ): ApkVersionInfo? {
        if (!file.exists() || file.length() <= 0L) return null
        val info = context.packageManager.getPackageArchiveInfoCompat(file.absolutePath)
        val version = info?.let {
            ApkVersionInfo(
                packageName = it.packageName,
                versionCode = it.longVersionCodeCompat(),
                versionName = it.versionName.orEmpty(),
            )
        }
        if (version != null) {
            onProgress(ApkDownloadProgress(message = "Cached ${file.name} ${version.versionLabel}"))
        } else {
            onProgress(ApkDownloadProgress(message = "Cached ${file.name} version metadata unreadable"))
        }
        return version
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

private data class ApkVersionInfo(
    val packageName: String,
    val versionCode: Long,
    val versionName: String,
) {
    val versionLabel: String = "$packageName v$versionName ($versionCode)"

    fun samePackageVersion(other: ApkVersionInfo): Boolean =
        packageName == other.packageName && versionCode == other.versionCode && versionName == other.versionName
}

private fun parseAaptPackageLine(line: String): ApkVersionInfo? {
    if (!line.startsWith("package:")) return null
    val packageName = line.extractAaptAttribute("name") ?: return null
    val versionCode = line.extractAaptAttribute("versionCode")?.toLongOrNull() ?: return null
    val versionName = line.extractAaptAttribute("versionName").orEmpty()
    return ApkVersionInfo(packageName = packageName, versionCode = versionCode, versionName = versionName)
}

private fun String.extractAaptAttribute(name: String): String? =
    Regex("$name='([^']*)'").find(this)?.groupValues?.getOrNull(1)

private fun shellSingleQuote(value: String): String = "'" + value.replace("'", "'\\''") + "'"

@Suppress("DEPRECATION")
private fun android.content.pm.PackageManager.getPackageArchiveInfoCompat(path: String): PackageInfo? =
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
        getPackageArchiveInfo(path, android.content.pm.PackageManager.PackageInfoFlags.of(0))
    } else {
        getPackageArchiveInfo(path, 0)
    }

@Suppress("DEPRECATION")
private fun PackageInfo.longVersionCodeCompat(): Long =
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) longVersionCode else versionCode.toLong()

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

package com.stellaclaw.stellacodex.data.ssh

import com.jcraft.jsch.JSch
import com.jcraft.jsch.JSchException
import com.jcraft.jsch.Logger
import com.jcraft.jsch.Session
import com.jcraft.jsch.UIKeyboardInteractive
import com.jcraft.jsch.UserInfo
import com.stellaclaw.stellacodex.domain.model.ConnectionProfile
import java.net.URI

class SshTunnelManager {
    private var activeTunnel: ActiveTunnel? = null

    @Synchronized
    fun resolveBaseUrl(profile: ConnectionProfile): String {
        val target = URI(profile.effectiveTargetUrl.trim())
        val signature = listOf(
            profile.sshHost.trim(),
            profile.sshPort.trim(),
            profile.sshUser.trim(),
            profile.effectiveTargetUrl.trim(),
        ).joinToString("|")

        activeTunnel?.let { tunnel ->
            if (tunnel.signature == signature && tunnel.session.isConnected) {
                return tunnel.baseUrl
            }
            tunnel.close()
            activeTunnel = null
        }

        val debugLog = SshDebugLog()
        JSch.setLogger(debugLog)
        val jsch = JSch()
        val privateKey = normalizePrivateKey(profile.sshPrivateKey)
        try {
            if (privateKey.isNotBlank()) {
                jsch.addIdentity(
                    "stellacodex-key",
                    privateKey.toByteArray(),
                    null,
                    profile.sshPassphrase.takeIf { it.isNotBlank() }?.toByteArray(),
                )
                debugLog.add("identity loaded: stellacodex-key, bytes=${privateKey.length}")
            } else {
                debugLog.add("no private key provided")
            }
        } catch (error: JSchException) {
            throw SshTunnelException(
                "Failed to parse SSH private key: ${error.message.orEmpty()}\n${debugLog.render()}",
                error,
            )
        }

        val port = profile.sshPort.trim().toIntOrNull() ?: 22
        val session = jsch.getSession(profile.sshUser.trim(), profile.sshHost.trim(), port)
        val password = profile.sshPassword.takeIf { it.isNotBlank() }
        if (password != null) {
            session.setPassword(password)
        }
        session.userInfo = StaticUserInfo(
            password = password,
            passphrase = profile.sshPassphrase.takeIf { it.isNotBlank() },
        )
        session.setConfig("StrictHostKeyChecking", "no")
        session.setConfig("PreferredAuthentications", "publickey,password,keyboard-interactive")
        session.setConfig("signature.ed25519", "com.jcraft.jsch.bc.SignatureEd25519")
        session.setConfig("signature.ed448", "com.jcraft.jsch.bc.SignatureEd448")
        session.setConfig(
            "PubkeyAcceptedAlgorithms",
            "ssh-ed25519,ecdsa-sha2-nistp256,ecdsa-sha2-nistp384,ecdsa-sha2-nistp521,rsa-sha2-512,rsa-sha2-256,ssh-rsa",
        )
        session.setConfig(
            "server_host_key",
            "ssh-ed25519,ecdsa-sha2-nistp256,ecdsa-sha2-nistp384,ecdsa-sha2-nistp521,rsa-sha2-512,rsa-sha2-256,ssh-rsa",
        )
        try {
            session.connect(10_000)
        } catch (error: JSchException) {
            throw SshTunnelException(
                authFailureMessage(error, privateKey.isNotBlank(), password != null, debugLog.render()),
                error,
            )
        }

        val targetPort = when {
            target.port > 0 -> target.port
            target.scheme == "https" -> 443
            else -> 80
        }
        val localPort = session.setPortForwardingL(
            "127.0.0.1",
            0,
            target.host,
            targetPort,
        )
        val basePath = target.path?.takeIf { it.isNotBlank() && it != "/" }?.trimEnd('/') ?: ""
        val baseUrl = "${target.scheme}://127.0.0.1:$localPort$basePath"
        val tunnel = ActiveTunnel(session, baseUrl, signature)
        activeTunnel = tunnel
        return baseUrl
    }

    @Synchronized
    fun close() {
        activeTunnel?.close()
        activeTunnel = null
    }

    private data class ActiveTunnel(
        val session: Session,
        val baseUrl: String,
        val signature: String,
    ) {
        fun close() {
            runCatching { session.disconnect() }
        }
    }
}

class SshTunnelException(message: String, cause: Throwable) : Exception(message, cause)

private fun normalizePrivateKey(value: String): String = value
    .trim()
    .replace("\r\n", "\n")
    .replace("\r", "\n")
    .replace("\\n", "\n")

private fun authFailureMessage(
    error: JSchException,
    hasPrivateKey: Boolean,
    hasPassword: Boolean,
    debugLog: String,
): String {
    val message = error.message.orEmpty()
    val base = if (!message.contains("Auth fail", ignoreCase = true)) {
        message.ifBlank { "SSH connection failed" }
    } else {
        buildString {
            append("SSH authentication failed")
            when {
                !hasPrivateKey && !hasPassword -> append(": enter an SSH password or paste the private key used by Termux.")
                !hasPrivateKey -> append(": the server appears to require public-key auth. Paste the same private key used by Termux.")
                else -> append(": the key was parsed, but the server rejected it. This may be key algorithm/signature compatibility or a passphrase mismatch.")
            }
            append(" Raw error: ").append(message)
        }
    }
    return "$base\n\nSSH debug log:\n$debugLog"
}

private class StaticUserInfo(
    private val password: String?,
    private val passphrase: String?,
) : UserInfo, UIKeyboardInteractive {
    override fun getPassword(): String? = password
    override fun getPassphrase(): String? = passphrase
    override fun promptPassword(message: String?): Boolean = password != null
    override fun promptPassphrase(message: String?): Boolean = passphrase != null
    override fun promptYesNo(message: String?): Boolean = true
    override fun showMessage(message: String?) = Unit

    override fun promptKeyboardInteractive(
        destination: String?,
        name: String?,
        instruction: String?,
        prompt: Array<out String>?,
        echo: BooleanArray?,
    ): Array<String>? {
        val value = password ?: return null
        return Array(prompt?.size ?: 0) { value }
    }
}

private class SshDebugLog : Logger {
    private val lines = ArrayDeque<String>()

    override fun isEnabled(level: Int): Boolean = true

    override fun log(level: Int, message: String?) {
        add("${levelName(level)}: ${message.orEmpty()}")
    }

    fun add(message: String) {
        lines.addLast(message)
        while (lines.size > 80) {
            lines.removeFirst()
        }
    }

    fun render(): String = lines.joinToString("\n").ifBlank { "<empty>" }

    private fun levelName(level: Int): String = when (level) {
        Logger.DEBUG -> "DEBUG"
        Logger.INFO -> "INFO"
        Logger.WARN -> "WARN"
        Logger.ERROR -> "ERROR"
        Logger.FATAL -> "FATAL"
        else -> level.toString()
    }
}

package com.stellaclaw.stellacodex.domain.model

enum class ConnectionMode(val wireName: String) {
    Direct("direct"),
    SshProxy("ssh_proxy"),
}

fun connectionModeFromWireName(value: String): ConnectionMode = when (value) {
    ConnectionMode.SshProxy.wireName -> ConnectionMode.SshProxy
    else -> ConnectionMode.Direct
}

data class ConnectionProfile(
    val name: String,
    val connectionMode: ConnectionMode,
    val baseUrl: String,
    val targetUrl: String,
    val sshHost: String,
    val sshPort: String,
    val sshUser: String,
    val sshPassword: String,
    val sshPrivateKey: String,
    val sshPassphrase: String,
    val token: String,
    val userName: String,
) {
    val effectiveTargetUrl: String
        get() = targetUrl.ifBlank { baseUrl }

    val isConfigured: Boolean
        get() = token.isNotBlank() && when (connectionMode) {
            ConnectionMode.Direct -> baseUrl.isNotBlank()
            ConnectionMode.SshProxy -> effectiveTargetUrl.isNotBlank() && sshHost.isNotBlank() && sshUser.isNotBlank()
        }
}

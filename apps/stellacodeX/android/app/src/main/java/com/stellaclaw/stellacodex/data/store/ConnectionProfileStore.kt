package com.stellaclaw.stellacodex.data.store

import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import com.stellaclaw.stellacodex.domain.model.ConnectionMode
import com.stellaclaw.stellacodex.domain.model.ConnectionProfile
import com.stellaclaw.stellacodex.domain.model.connectionModeFromWireName
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.map

class ConnectionProfileStore(
    private val dataStore: DataStore<Preferences>,
) {
    val profile: Flow<ConnectionProfile> = dataStore.data.map { preferences ->
        ConnectionProfile(
            name = preferences[NameKey].orEmpty(),
            connectionMode = connectionModeFromWireName(preferences[ConnectionModeKey].orEmpty()),
            baseUrl = preferences[BaseUrlKey].orEmpty(),
            targetUrl = preferences[TargetUrlKey].orEmpty(),
            sshHost = preferences[SshHostKey].orEmpty(),
            sshPort = preferences[SshPortKey] ?: "22",
            sshUser = preferences[SshUserKey].orEmpty(),
            sshPassword = preferences[SshPasswordKey].orEmpty(),
            sshPrivateKey = preferences[SshPrivateKeyKey].orEmpty(),
            sshPassphrase = preferences[SshPassphraseKey].orEmpty(),
            token = preferences[TokenKey].orEmpty(),
        )
    }

    suspend fun save(profile: ConnectionProfile) {
        dataStore.edit { preferences ->
            preferences[NameKey] = profile.name.trim().ifBlank { "Stellaclaw" }
            preferences[ConnectionModeKey] = profile.connectionMode.wireName
            preferences[BaseUrlKey] = profile.baseUrl.trim().trimEnd('/')
            preferences[TargetUrlKey] = profile.effectiveTargetUrl.trim().trimEnd('/')
            preferences[SshHostKey] = profile.sshHost.trim()
            preferences[SshPortKey] = profile.sshPort.trim().ifBlank { "22" }
            preferences[SshUserKey] = profile.sshUser.trim()
            preferences[SshPasswordKey] = profile.sshPassword
            preferences[SshPrivateKeyKey] = profile.sshPrivateKey.trim()
            preferences[SshPassphraseKey] = profile.sshPassphrase
            preferences[TokenKey] = profile.token.trim()
        }
    }

    private companion object {
        val NameKey = stringPreferencesKey("name")
        val ConnectionModeKey = stringPreferencesKey("connection_mode")
        val BaseUrlKey = stringPreferencesKey("base_url")
        val TargetUrlKey = stringPreferencesKey("target_url")
        val SshHostKey = stringPreferencesKey("ssh_host")
        val SshPortKey = stringPreferencesKey("ssh_port")
        val SshUserKey = stringPreferencesKey("ssh_user")
        val SshPasswordKey = stringPreferencesKey("ssh_password")
        val SshPrivateKeyKey = stringPreferencesKey("ssh_private_key")
        val SshPassphraseKey = stringPreferencesKey("ssh_passphrase")
        val TokenKey = stringPreferencesKey("token")
    }
}

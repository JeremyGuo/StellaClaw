package com.stellaclaw.stellacodex.ui.connections

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.stellaclaw.stellacodex.core.result.AppResult
import com.stellaclaw.stellacodex.core.result.userMessage
import com.stellaclaw.stellacodex.data.api.StellaclawApi
import com.stellaclaw.stellacodex.data.store.ConnectionProfileStore
import com.stellaclaw.stellacodex.data.store.connectionDataStore
import com.stellaclaw.stellacodex.domain.model.ConnectionMode
import com.stellaclaw.stellacodex.domain.model.ConnectionProfile
import com.stellaclaw.stellacodex.domain.model.ModelInfo
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch

class ConnectionViewModel(application: Application) : AndroidViewModel(application) {
    private val store = ConnectionProfileStore(application.connectionDataStore)
    private val api = StellaclawApi()

    private val mutableState = MutableStateFlow(ConnectionUiState())
    val state: StateFlow<ConnectionUiState> = mutableState.asStateFlow()

    init {
        viewModelScope.launch {
            store.profile.collect { profile ->
                mutableState.update {
                    it.copy(
                        name = profile.name,
                        connectionMode = profile.connectionMode,
                        baseUrl = profile.baseUrl,
                        targetUrl = profile.targetUrl,
                        sshHost = profile.sshHost,
                        sshPort = profile.sshPort,
                        sshUser = profile.sshUser,
                        sshPassword = profile.sshPassword,
                        sshPrivateKey = profile.sshPrivateKey,
                        sshPassphrase = profile.sshPassphrase,
                        token = profile.token,
                        hasSavedProfile = profile.isConfigured,
                    )
                }
            }
        }
    }

    fun onNameChanged(value: String) = updateForm { it.copy(name = value) }
    fun onConnectionModeChanged(value: ConnectionMode) = updateForm { it.copy(connectionMode = value) }
    fun onBaseUrlChanged(value: String) = updateForm { it.copy(baseUrl = value) }
    fun onTargetUrlChanged(value: String) = updateForm { it.copy(targetUrl = value) }
    fun onSshHostChanged(value: String) = updateForm { it.copy(sshHost = value) }
    fun onSshPortChanged(value: String) = updateForm { it.copy(sshPort = value.filter(Char::isDigit)) }
    fun onSshUserChanged(value: String) = updateForm { it.copy(sshUser = value) }
    fun onSshPasswordChanged(value: String) = updateForm { it.copy(sshPassword = value) }
    fun onSshPrivateKeyChanged(value: String) = updateForm { it.copy(sshPrivateKey = value) }
    fun onSshPassphraseChanged(value: String) = updateForm { it.copy(sshPassphrase = value) }
    fun onTokenChanged(value: String) = updateForm { it.copy(token = value) }

    private fun updateForm(update: (ConnectionUiState) -> ConnectionUiState) {
        mutableState.update { update(it).copy(error = null, successMessage = null) }
    }

    fun saveAndValidate(onValidated: () -> Unit) {
        val profile = state.value.toProfile()
        if (!profile.isConfigured) {
            mutableState.update {
                it.copy(error = "Fill server token and the required ${if (profile.connectionMode == ConnectionMode.SshProxy) "SSH proxy" else "direct"} fields.")
            }
            return
        }

        viewModelScope.launch {
            mutableState.update { it.copy(isValidating = true, error = null, successMessage = null) }
            when (val result = api.loadModels(profile)) {
                is AppResult.Ok -> {
                    store.save(profile)
                    mutableState.update {
                        it.copy(
                            isValidating = false,
                            hasSavedProfile = true,
                            models = result.value,
                            successMessage = "Connected. ${result.value.size} models available.",
                        )
                    }
                    onValidated()
                }
                is AppResult.Err -> {
                    mutableState.update {
                        it.copy(
                            isValidating = false,
                            error = result.error.userMessage(),
                        )
                    }
                }
            }
        }
    }

    fun continueWithoutValidation(onContinue: () -> Unit) {
        onContinue()
    }

    private fun ConnectionUiState.toProfile(): ConnectionProfile = ConnectionProfile(
        name = name.ifBlank { "Stellaclaw" },
        connectionMode = connectionMode,
        baseUrl = baseUrl,
        targetUrl = targetUrl,
        sshHost = sshHost,
        sshPort = sshPort.ifBlank { "22" },
        sshUser = sshUser,
        sshPassword = sshPassword,
        sshPrivateKey = sshPrivateKey,
        sshPassphrase = sshPassphrase,
        token = token,
    )
}

data class ConnectionUiState(
    val name: String = "Stellaclaw",
    val connectionMode: ConnectionMode = ConnectionMode.SshProxy,
    val baseUrl: String = "http://127.0.0.1:3111",
    val targetUrl: String = "http://127.0.0.1:3111",
    val sshHost: String = "",
    val sshPort: String = "22",
    val sshUser: String = "",
    val sshPassword: String = "",
    val sshPrivateKey: String = "",
    val sshPassphrase: String = "",
    val token: String = "",
    val hasSavedProfile: Boolean = false,
    val isValidating: Boolean = false,
    val models: List<ModelInfo> = emptyList(),
    val error: String? = null,
    val successMessage: String? = null,
)

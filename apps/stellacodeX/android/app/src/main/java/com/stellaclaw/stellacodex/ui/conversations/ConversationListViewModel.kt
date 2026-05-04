package com.stellaclaw.stellacodex.ui.conversations

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.stellaclaw.stellacodex.core.result.AppResult
import com.stellaclaw.stellacodex.core.result.userMessage
import com.stellaclaw.stellacodex.data.api.StellaclawApi
import com.stellaclaw.stellacodex.data.network.NetworkMonitor
import com.stellaclaw.stellacodex.data.network.NetworkState
import com.stellaclaw.stellacodex.data.store.ConnectionProfileStore
import com.stellaclaw.stellacodex.data.store.connectionDataStore
import com.stellaclaw.stellacodex.domain.model.ConnectionProfile
import com.stellaclaw.stellacodex.domain.model.ConversationSummary
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch

class ConversationListViewModel(application: Application) : AndroidViewModel(application) {
    private val store = ConnectionProfileStore(application.connectionDataStore)
    private val api = StellaclawApi()

    private val mutableState = MutableStateFlow(ConversationListUiState())
    val state: StateFlow<ConversationListUiState> = mutableState.asStateFlow()

    init {
        NetworkMonitor.start(application)
        refresh()
        viewModelScope.launch {
            NetworkMonitor.state.collect { networkState ->
                if (networkState == NetworkState.Available && state.value.error != null) {
                    refresh(showLoading = false)
                }
            }
        }
    }

    fun refresh(showLoading: Boolean = true) {
        viewModelScope.launch {
            if (showLoading) {
                mutableState.update { it.copy(isLoading = true, error = null) }
            } else {
                mutableState.update { it.copy(error = null) }
            }
            val profile = store.profile.first()
            if (!profile.isConfigured) {
                mutableState.update {
                    it.copy(
                        isLoading = false,
                        activeConnectionName = profile.name.ifBlank { "Stellaclaw" },
                        error = "Connection profile is incomplete. Go back to connection setup.",
                    )
                }
                return@launch
            }
            when (val result = api.loadConversations(profile)) {
                is AppResult.Ok -> mutableState.update {
                    it.copy(
                        isLoading = false,
                        activeConnectionName = profile.displayName(),
                        conversations = result.value,
                        error = null,
                    )
                }
                is AppResult.Err -> mutableState.update {
                    it.copy(
                        isLoading = false,
                        activeConnectionName = profile.displayName(),
                        error = result.error.userMessage(),
                    )
                }
            }
        }
    }

    fun createConversation() {
        if (state.value.isCreating) return
        viewModelScope.launch {
            mutableState.update { it.copy(isCreating = true, error = null) }
            val profile = store.profile.first()
            if (!profile.isConfigured) {
                mutableState.update {
                    it.copy(
                        isCreating = false,
                        error = "Connection profile is incomplete. Go back to connection setup.",
                    )
                }
                return@launch
            }
            when (val result = api.createConversation(profile)) {
                is AppResult.Ok -> {
                    mutableState.update {
                        it.copy(
                            isCreating = false,
                            pendingOpenConversationId = result.value,
                        )
                    }
                    refresh()
                }
                is AppResult.Err -> mutableState.update {
                    it.copy(
                        isCreating = false,
                        error = result.error.userMessage(),
                    )
                }
            }
        }
    }

    fun consumePendingOpenConversation() {
        mutableState.update { it.copy(pendingOpenConversationId = null) }
    }

    private fun ConnectionProfile.displayName(): String = name.ifBlank { sshHost.ifBlank { baseUrl.ifBlank { "Stellaclaw" } } }
}

data class ConversationListUiState(
    val activeConnectionName: String = "Stellaclaw",
    val isLoading: Boolean = false,
    val isCreating: Boolean = false,
    val conversations: List<ConversationSummary> = emptyList(),
    val error: String? = null,
    val pendingOpenConversationId: String? = null,
)

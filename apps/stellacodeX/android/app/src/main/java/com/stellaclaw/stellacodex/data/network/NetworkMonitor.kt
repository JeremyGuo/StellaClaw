package com.stellaclaw.stellacodex.data.network

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import com.stellaclaw.stellacodex.data.log.AppLogStore
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

sealed interface NetworkState {
    data object Available : NetworkState
    data object Lost : NetworkState
    data object Unavailable : NetworkState
}

object NetworkMonitor {
    private val mutableState = MutableStateFlow<NetworkState>(NetworkState.Unavailable)
    val state: StateFlow<NetworkState> = mutableState.asStateFlow()

    private var started = false
    private var appContext: Context? = null
    private var connectivityManager: ConnectivityManager? = null
    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            updateState(NetworkState.Available, "callback available")
        }

        override fun onLost(network: Network) {
            updateState(if (hasUsableNetwork()) NetworkState.Available else NetworkState.Lost, "callback lost")
        }

        override fun onUnavailable() {
            updateState(NetworkState.Unavailable, "callback unavailable")
        }
    }

    @Synchronized
    fun start(context: Context) {
        if (started) return
        appContext = context.applicationContext
        val manager = context.applicationContext.getSystemService(ConnectivityManager::class.java)
        if (manager == null) {
            log("start skipped: ConnectivityManager unavailable")
            return
        }
        connectivityManager = manager
        updateState(if (manager.activeNetworkHasInternet()) NetworkState.Available else NetworkState.Unavailable, "initial")
        val registered = runCatching {
            manager.registerDefaultNetworkCallback(callback)
        }.onFailure { error ->
            log("register callback failed ${error::class.java.simpleName}: ${error.message.orEmpty()}")
        }.isSuccess
        started = registered
        log("start registered=$registered state=${mutableState.value}")
    }

    @Synchronized
    fun stop() {
        if (!started) return
        runCatching { connectivityManager?.unregisterNetworkCallback(callback) }
            .onFailure { error -> log("unregister callback failed ${error::class.java.simpleName}: ${error.message.orEmpty()}") }
        connectivityManager = null
        started = false
        log("stopped")
    }

    fun isAvailable(): Boolean = state.value == NetworkState.Available

    private fun hasUsableNetwork(): Boolean = connectivityManager?.activeNetworkHasInternet() == true

    private fun ConnectivityManager.activeNetworkHasInternet(): Boolean = runCatching {
        val network = activeNetwork ?: return@runCatching false
        val capabilities = getNetworkCapabilities(network) ?: return@runCatching false
        capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
    }.onFailure { error ->
        log("active network check failed ${error::class.java.simpleName}: ${error.message.orEmpty()}")
    }.getOrDefault(false)

    private fun updateState(next: NetworkState, reason: String) {
        val previous = mutableState.value
        mutableState.value = next
        if (previous != next) {
            log("state $previous -> $next reason=$reason")
        }
    }

    private fun log(message: String) {
        appContext?.let { AppLogStore.append(it, "network", message) }
    }
}

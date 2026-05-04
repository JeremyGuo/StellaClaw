package com.stellaclaw.stellacodex.data.network

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
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
    private var connectivityManager: ConnectivityManager? = null
    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            mutableState.value = NetworkState.Available
        }

        override fun onLost(network: Network) {
            mutableState.value = if (hasUsableNetwork()) NetworkState.Available else NetworkState.Lost
        }

        override fun onUnavailable() {
            mutableState.value = NetworkState.Unavailable
        }
    }

    @Synchronized
    fun start(context: Context) {
        if (started) return
        val manager = context.applicationContext.getSystemService(ConnectivityManager::class.java) ?: return
        connectivityManager = manager
        mutableState.value = if (manager.activeNetworkHasInternet()) NetworkState.Available else NetworkState.Unavailable
        val registered = runCatching {
            manager.registerDefaultNetworkCallback(callback)
        }.isSuccess
        started = registered
    }

    @Synchronized
    fun stop() {
        if (!started) return
        runCatching { connectivityManager?.unregisterNetworkCallback(callback) }
        connectivityManager = null
        started = false
    }

    fun isAvailable(): Boolean = state.value == NetworkState.Available

    private fun hasUsableNetwork(): Boolean = connectivityManager?.activeNetworkHasInternet() == true

    private fun ConnectivityManager.activeNetworkHasInternet(): Boolean = runCatching {
        val network = activeNetwork ?: return@runCatching false
        val capabilities = getNetworkCapabilities(network) ?: return@runCatching false
        capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
    }.getOrDefault(false)
}

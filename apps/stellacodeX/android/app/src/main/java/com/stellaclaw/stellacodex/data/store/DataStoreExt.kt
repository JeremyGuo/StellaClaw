package com.stellaclaw.stellacodex.data.store

import android.content.Context
import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.preferencesDataStore

val Context.connectionDataStore: DataStore<Preferences> by preferencesDataStore(
    name = "connection_profile",
)

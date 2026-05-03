package com.stellaclaw.stellacodex.ui.logs

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import com.stellaclaw.stellacodex.data.log.AppLogStore
import kotlinx.coroutines.flow.StateFlow

class LogsViewModel(application: Application) : AndroidViewModel(application) {
    val text: StateFlow<String> = AppLogStore.text

    init {
        AppLogStore.initialize(application)
    }

    fun clear() {
        AppLogStore.clear(getApplication())
    }
}

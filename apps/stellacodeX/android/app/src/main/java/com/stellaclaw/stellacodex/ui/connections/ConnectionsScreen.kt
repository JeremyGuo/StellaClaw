package com.stellaclaw.stellacodex.ui.connections

import android.app.Application
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import com.stellaclaw.stellacodex.domain.model.ConnectionMode

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ConnectionsScreen(onContinue: () -> Unit) {
    val application = LocalContext.current.applicationContext as Application
    val viewModel: ConnectionViewModel = viewModel(
        factory = viewModelFactory {
            initializer { ConnectionViewModel(application) }
        },
    )
    val state by viewModel.state.collectAsStateWithLifecycle()
    var showToken by remember { mutableStateOf(false) }
    var showSshSecret by remember { mutableStateOf(false) }

    Scaffold(
        topBar = { TopAppBar(title = { Text("StellacodeX") }) },
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .verticalScroll(rememberScrollState())
                .padding(24.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Text("Connect to StellacodeX", style = MaterialTheme.typography.headlineSmall)
            Text(
                text = "Client name: StellacodeX. Username is sent as Speaker metadata so the assistant can distinguish people in shared conversations.",
                style = MaterialTheme.typography.bodyMedium,
            )

            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                FilterChip(
                    selected = state.connectionMode == ConnectionMode.SshProxy,
                    onClick = { viewModel.onConnectionModeChanged(ConnectionMode.SshProxy) },
                    label = { Text("SSH proxy") },
                )
                FilterChip(
                    selected = state.connectionMode == ConnectionMode.Direct,
                    onClick = { viewModel.onConnectionModeChanged(ConnectionMode.Direct) },
                    label = { Text("Direct") },
                )
            }

            OutlinedTextField(
                value = state.name,
                onValueChange = viewModel::onNameChanged,
                modifier = Modifier.fillMaxWidth(),
                label = { Text("Name") },
                singleLine = true,
            )

            if (state.connectionMode == ConnectionMode.Direct) {
                OutlinedTextField(
                    value = state.baseUrl,
                    onValueChange = viewModel::onBaseUrlChanged,
                    modifier = Modifier.fillMaxWidth(),
                    label = { Text("Server base URL") },
                    placeholder = { Text("http://server-ip:3111") },
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                    singleLine = true,
                )
            } else {
                SshProxyFields(
                    state = state,
                    showSshSecret = showSshSecret,
                    onToggleSecret = { showSshSecret = !showSshSecret },
                    viewModel = viewModel,
                )
            }

            OutlinedTextField(
                value = state.userName,
                onValueChange = viewModel::onUserNameChanged,
                modifier = Modifier.fillMaxWidth(),
                label = { Text("Username") },
                placeholder = { Text("workspace-user") },
                singleLine = true,
            )

            OutlinedTextField(
                value = state.token,
                onValueChange = viewModel::onTokenChanged,
                modifier = Modifier.fillMaxWidth(),
                label = { Text("Web bearer token") },
                visualTransformation = if (showToken) VisualTransformation.None else PasswordVisualTransformation(),
                singleLine = true,
                trailingIcon = {
                    TextButton(onClick = { showToken = !showToken }) {
                        Text(if (showToken) "Hide" else "Show")
                    }
                },
            )

            state.error?.let { message ->
                Text(message, color = MaterialTheme.colorScheme.error)
            }
            state.successMessage?.let { message ->
                Text(message, color = MaterialTheme.colorScheme.secondary)
            }

            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(12.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Button(
                    onClick = { viewModel.saveAndValidate(onContinue) },
                    enabled = !state.isValidating,
                ) {
                    Text(if (state.hasSavedProfile) "Validate and continue" else "Save and continue")
                }
                if (state.isValidating) CircularProgressIndicator()
            }

            if (state.hasSavedProfile) {
                TextButton(onClick = { viewModel.continueWithoutValidation(onContinue) }) {
                    Text("Continue with saved profile")
                }
            }

            if (state.models.isNotEmpty()) {
                Card(modifier = Modifier.fillMaxWidth()) {
                    Column(
                        modifier = Modifier.padding(12.dp),
                        verticalArrangement = Arrangement.spacedBy(4.dp),
                    ) {
                        Text("Available models", style = MaterialTheme.typography.titleSmall)
                        state.models.take(5).forEach { model ->
                            Text("${model.alias} · ${model.modelName} · ${model.providerType}")
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun SshProxyFields(
    state: ConnectionUiState,
    showSshSecret: Boolean,
    onToggleSecret: () -> Unit,
    viewModel: ConnectionViewModel,
) {
    OutlinedTextField(
        value = state.targetUrl,
        onValueChange = viewModel::onTargetUrlChanged,
        modifier = Modifier.fillMaxWidth(),
        label = { Text("Target URL on server") },
        placeholder = { Text("http://127.0.0.1:3111") },
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
        singleLine = true,
    )
    Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
        OutlinedTextField(
            value = state.sshHost,
            onValueChange = viewModel::onSshHostChanged,
            modifier = Modifier.weight(1f),
            label = { Text("SSH host") },
            placeholder = { Text("server.example.com") },
            singleLine = true,
        )
        OutlinedTextField(
            value = state.sshPort,
            onValueChange = viewModel::onSshPortChanged,
            modifier = Modifier.weight(0.45f),
            label = { Text("Port") },
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
            singleLine = true,
        )
    }
    OutlinedTextField(
        value = state.sshUser,
        onValueChange = viewModel::onSshUserChanged,
        modifier = Modifier.fillMaxWidth(),
        label = { Text("SSH user") },
        singleLine = true,
    )
    OutlinedTextField(
        value = state.sshPassword,
        onValueChange = viewModel::onSshPasswordChanged,
        modifier = Modifier.fillMaxWidth(),
        label = { Text("SSH password (optional)") },
        visualTransformation = if (showSshSecret) VisualTransformation.None else PasswordVisualTransformation(),
        singleLine = true,
        trailingIcon = {
            TextButton(onClick = onToggleSecret) {
                Text(if (showSshSecret) "Hide" else "Show")
            }
        },
    )
    OutlinedTextField(
        value = state.sshPrivateKey,
        onValueChange = viewModel::onSshPrivateKeyChanged,
        modifier = Modifier.fillMaxWidth(),
        label = { Text("Private key PEM (optional)") },
        placeholder = { Text("-----BEGIN OPENSSH PRIVATE KEY-----") },
        minLines = 3,
        maxLines = 6,
        visualTransformation = if (showSshSecret) VisualTransformation.None else PasswordVisualTransformation(),
    )
    OutlinedTextField(
        value = state.sshPassphrase,
        onValueChange = viewModel::onSshPassphraseChanged,
        modifier = Modifier.fillMaxWidth(),
        label = { Text("Key passphrase (optional)") },
        visualTransformation = if (showSshSecret) VisualTransformation.None else PasswordVisualTransformation(),
        singleLine = true,
    )
}

package app.gwar.android.ui

import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import app.gwar.android.ConnectForm
import app.gwar.android.Problem
import app.gwar.android.R
import app.gwar.android.UiState

@Composable
fun ConnectScreen(
    state: UiState,
    onForm: (ConnectForm) -> Unit,
    onConnect: () -> Unit,
    onTrustNewKey: (String) -> Unit,
    onDismissProblem: () -> Unit,
) {
    val form = state.form
    var more by rememberSaveable { mutableStateOf(form.insecureLan || form.serverPassword.isNotEmpty()) }
    val canConnect = !state.connecting && form.address.isNotBlank() && form.nickname.isNotBlank()

    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        Box(
            Modifier
                .fillMaxSize()
                .safeDrawingPadding()
                .imePadding()
                .verticalScroll(rememberScrollState()),
            contentAlignment = Alignment.Center,
        ) {
            Column(
                Modifier
                    .widthIn(max = 420.dp)
                    .fillMaxWidth()
                    .padding(24.dp),
                verticalArrangement = Arrangement.spacedBy(14.dp),
            ) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Image(painterResource(R.drawable.ic_gwar_mark), contentDescription = null, modifier = Modifier.size(44.dp))
                    Spacer(Modifier.size(12.dp))
                    Column {
                        Text(stringResource(R.string.app_name), style = MaterialTheme.typography.headlineSmall, fontWeight = FontWeight.SemiBold)
                        Text(
                            stringResource(R.string.connect_subtitle),
                            style = MaterialTheme.typography.bodyMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
                Spacer(Modifier.height(8.dp))
                OutlinedTextField(
                    value = form.address,
                    onValueChange = { onForm(form.copy(address = it)) },
                    label = { Text(stringResource(R.string.connect_address)) },
                    placeholder = { Text("voice.example.com") },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri, imeAction = ImeAction.Next, autoCorrectEnabled = false),
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = form.nickname,
                    onValueChange = { onForm(form.copy(nickname = it)) },
                    label = { Text(stringResource(R.string.connect_nickname)) },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(imeAction = ImeAction.Go),
                    keyboardActions = KeyboardActions(onGo = { if (canConnect) onConnect() }),
                    modifier = Modifier.fillMaxWidth(),
                )
                if (more) {
                    OutlinedTextField(
                        value = form.serverPassword,
                        onValueChange = { onForm(form.copy(serverPassword = it)) },
                        label = { Text(stringResource(R.string.connect_server_password)) },
                        singleLine = true,
                        visualTransformation = PasswordVisualTransformation(),
                        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
                        modifier = Modifier.fillMaxWidth(),
                    )
                    Row(
                        Modifier.toggleable(
                            value = form.insecureLan,
                            role = Role.Switch,
                            onValueChange = { onForm(form.copy(insecureLan = it)) },
                        ),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Column(Modifier.weight(1f)) {
                            Text(stringResource(R.string.connect_insecure_lan), style = MaterialTheme.typography.bodyLarge)
                            Text(
                                stringResource(R.string.connect_insecure_lan_hint),
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                        Spacer(Modifier.size(12.dp))
                        Switch(checked = form.insecureLan, onCheckedChange = null)
                    }
                } else {
                    TextButton(onClick = { more = true }) { Text(stringResource(R.string.connect_more)) }
                }

                when (val problem = state.problem) {
                    is Problem.Failed -> ProblemText(describe(problem.error))
                    is Problem.Closed -> ProblemText(describe(problem.reason))
                    else -> {}
                }

                Button(onClick = onConnect, enabled = canConnect, modifier = Modifier.fillMaxWidth().height(48.dp)) {
                    if (state.connecting) {
                        CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    } else {
                        Text(stringResource(R.string.connect_button))
                    }
                }
                Text(
                    stringResource(R.string.connect_your_id, state.uid.take(12)),
                    style = MaterialTheme.typography.bodySmall,
                    fontFamily = FontFamily.Monospace,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }

    val changed = state.problem as? Problem.IdentityChanged
    if (changed != null) {
        AlertDialog(
            onDismissRequest = onDismissProblem,
            title = { Text(stringResource(R.string.identity_changed_title)) },
            text = { Text(stringResource(R.string.identity_changed_body, changed.endpoint)) },
            confirmButton = {
                TextButton(onClick = { onTrustNewKey(changed.endpoint) }) {
                    Text(stringResource(R.string.identity_changed_trust), color = MaterialTheme.colorScheme.error)
                }
            },
            dismissButton = { TextButton(onClick = onDismissProblem) { Text(stringResource(R.string.cancel)) } },
        )
    }
}

@Composable
private fun ProblemText(text: String) {
    Surface(color = MaterialTheme.colorScheme.errorContainer, shape = RoundedCornerShape(10.dp)) {
        Text(
            text,
            color = MaterialTheme.colorScheme.onErrorContainer,
            style = MaterialTheme.typography.bodyMedium,
            modifier = Modifier.padding(12.dp).fillMaxWidth(),
        )
    }
}

package app.gwar.android.ui

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.lifecycle.compose.LifecycleResumeEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import app.gwar.android.GwarClient

@Composable
fun GwarRoot(client: GwarClient) {
    val state by client.state.collectAsStateWithLifecycle()
    val context = LocalContext.current

    // Asked from the output menu when a Bluetooth headset is wanted but not allowed;
    // checked again on return to the app, e.g. from the system settings.
    val bluetoothDenied = {
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.S &&
            context.checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) != PackageManager.PERMISSION_GRANTED
    }
    var bluetoothMissing by remember { mutableStateOf(bluetoothDenied()) }
    val bluetooth = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        bluetoothMissing = !granted
        client.refreshOutputs()
    }
    LifecycleResumeEffect(Unit) {
        bluetoothMissing = bluetoothDenied()
        client.refreshOutputs()
        onPauseOrDispose {}
    }

    // A join waiting for the permission answer: channel id and password.
    var pendingJoin by rememberSaveable { mutableStateOf<Pair<Long, String?>?>(null) }
    val permissions = rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { result ->
        val (channel, password) = pendingJoin ?: return@rememberLauncherForActivityResult
        pendingJoin = null
        bluetoothMissing = bluetoothDenied()
        client.joinChannel(channel.toUInt(), password, withVoice = result[Manifest.permission.RECORD_AUDIO] == true)
    }
    val join = { channel: UInt, password: String? ->
        if (context.checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED) {
            client.joinChannel(channel, password)
        } else {
            pendingJoin = channel.toLong() to password
            // The call's ongoing notification needs its own permission from Android 13.
            val wanted = buildList {
                add(Manifest.permission.RECORD_AUDIO)
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) add(Manifest.permission.POST_NOTIFICATIONS)
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) add(Manifest.permission.BLUETOOTH_CONNECT)
            }
            permissions.launch(wanted.toTypedArray())
        }
    }

    val view = state.view
    if (view == null) {
        ConnectScreen(
            state = state,
            onForm = client::updateForm,
            onConnect = client::connect,
            onTrustNewKey = client::trustNewKey,
            onDismissProblem = client::dismissProblem,
        )
        return
    }
    val open = state.openChannel?.let { id -> view.channels.firstOrNull { it.id == id } }
    if (open != null) {
        BackHandler(onBack = client::closeChat)
        ChatScreen(
            channel = open,
            chat = state.chats[open.id],
            notice = state.notice,
            onBack = client::closeChat,
            onSend = { text -> client.send(open.id, text) },
            onLoadOlder = { client.loadOlder(open.id) },
            onDismissNotice = client::dismissNotice,
        )
    } else {
        ServerScreen(
            view = view,
            voice = state.voice,
            muted = state.muted,
            deafened = state.deafened,
            outputs = state.outputs,
            output = state.output,
            onChooseOutput = client::chooseOutput,
            onAllowBluetooth = if (bluetoothMissing) {
                { bluetooth.launch(Manifest.permission.BLUETOOTH_CONNECT) }
            } else {
                null
            },
            reconnecting = state.reconnecting,
            notice = state.notice,
            onOpenChat = client::openChat,
            onJoin = join,
            onLeave = client::leaveChannel,
            onToggleMute = client::toggleMute,
            onToggleDeafen = client::toggleDeafen,
            onDisconnect = client::disconnect,
            onDismissNotice = client::dismissNotice,
        )
    }
}

package app.gwar.android.ui

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.height
import androidx.compose.material.icons.outlined.MoreVert
import androidx.compose.material3.Checkbox
import androidx.compose.material3.Slider
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.Login
import androidx.compose.material.icons.automirrored.outlined.Logout
import androidx.compose.material.icons.outlined.EnhancedEncryption
import androidx.compose.material.icons.automirrored.outlined.VolumeUp
import androidx.compose.material.icons.outlined.Bluetooth
import androidx.compose.material.icons.outlined.CallEnd
import androidx.compose.material.icons.outlined.Check
import androidx.compose.material.icons.outlined.Headphones
import androidx.compose.material.icons.outlined.PhoneInTalk
import androidx.compose.material.icons.outlined.Usb
import androidx.compose.material.icons.outlined.Headset
import androidx.compose.material.icons.outlined.HeadsetOff
import androidx.compose.material.icons.outlined.Mic
import androidx.compose.material.icons.outlined.Lock
import androidx.compose.material.icons.outlined.MicOff
import androidx.compose.material.icons.outlined.Tag
import androidx.compose.material.icons.outlined.VisibilityOff
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Badge
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import app.gwar.android.R
import app.gwar.android.AudioOutput
import app.gwar.android.DEFAULT_THRESHOLD_DB
import app.gwar.android.Notice
import app.gwar.android.OutputKind
import app.gwar.android.VoiceState
import app.gwar.core.VoiceLink
import app.gwar.core.ChannelView
import app.gwar.core.ClientPlatform
import app.gwar.core.ClientView
import app.gwar.core.ServerView

private const val INDENT = 18

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ServerScreen(
    view: ServerView,
    voice: VoiceState?,
    muted: Boolean,
    deafened: Boolean,
    outputs: List<AudioOutput>,
    output: AudioOutput?,
    pushToTalk: Boolean,
    talkHeld: Boolean,
    onSetPushToTalk: (Boolean) -> Unit,
    thresholdDb: Float,
    onSetThreshold: (Float) -> Unit,
    onHoldToTalk: (Boolean) -> Unit,
    onChooseOutput: (OutputKind) -> Unit,
    /** Set when Bluetooth headsets need a permission first. */
    onAllowBluetooth: (() -> Unit)?,
    reconnecting: Boolean,
    notice: Notice?,
    onOpenChat: (UInt) -> Unit,
    onJoin: (UInt, String?) -> Unit,
    onLeave: () -> Unit,
    onToggleMute: () -> Unit,
    onToggleDeafen: () -> Unit,
    onDisconnect: () -> Unit,
    onDismissNotice: () -> Unit,
) {
    val snackbar = remember { SnackbarHostState() }
    NoticeEffect(notice, snackbar, onDismissNotice)
    var passwordFor by rememberSaveable { mutableStateOf<Long?>(null) }
    val myChannel = view.myChannel?.let { id -> view.channels.firstOrNull { it.id == id } }

    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Column {
                        Text(view.name, maxLines = 1, overflow = TextOverflow.Ellipsis, fontWeight = FontWeight.SemiBold)
                        Text(
                            if (reconnecting) stringResource(R.string.server_reconnecting) else view.label,
                            style = MaterialTheme.typography.bodySmall,
                            color = if (reconnecting) GwarColors.warn else MaterialTheme.colorScheme.onSurfaceVariant,
                            maxLines = 1,
                        )
                    }
                },
                actions = {
                    IconButton(onClick = onDisconnect) {
                        Icon(Icons.AutoMirrored.Outlined.Logout, contentDescription = stringResource(R.string.server_disconnect))
                    }
                },
            )
        },
        bottomBar = {
            if (myChannel != null) {
                Column {
                    if (voice != null && pushToTalk && !reconnecting) TalkButton(talkHeld, onHoldToTalk)
                    VoiceBar(
                        channelName = myChannel.name,
                        voice = voice,
                        muted = muted,
                        deafened = deafened,
                        reconnecting = reconnecting,
                        output = output,
                        outputs = outputs,
                        onChooseOutput = onChooseOutput,
                        onAllowBluetooth = onAllowBluetooth,
                        pushToTalk = pushToTalk,
                        onSetPushToTalk = onSetPushToTalk,
                        thresholdDb = thresholdDb,
                        onSetThreshold = onSetThreshold,
                        onToggleMute = onToggleMute,
                        onToggleDeafen = onToggleDeafen,
                        onLeave = onLeave,
                    )
                }
            }
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        LazyColumn(Modifier.fillMaxSize().padding(padding)) {
            if (view.welcomeText.isNotBlank()) {
                item(key = "welcome") {
                    Text(
                        view.welcomeText,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
                    )
                }
            }
            for (channel in view.channels) {
                item(key = "c${channel.id}") {
                    ChannelRow(
                        channel = channel,
                        mine = channel.id == view.myChannel,
                        onOpen = { onOpenChat(channel.id) },
                        onJoin = {
                            if (channel.hasPassword) passwordFor = channel.id.toLong() else onJoin(channel.id, null)
                        },
                    )
                }
                items(channel.clients, key = { "u${it.id}" }) { client ->
                    ClientRow(client, depth = channel.depth.toInt() + 1)
                }
            }
            if (view.lobby.isNotEmpty()) {
                item(key = "lobby") {
                    HorizontalDivider(Modifier.padding(top = 8.dp), color = MaterialTheme.colorScheme.outlineVariant)
                    Text(
                        stringResource(R.string.server_lobby, view.lobby.size),
                        style = MaterialTheme.typography.labelMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.padding(start = 16.dp, top = 12.dp, bottom = 4.dp),
                    )
                }
                items(view.lobby, key = { "l${it.id}" }) { client -> ClientRow(client, depth = 0) }
            }
        }
    }

    passwordFor?.let { id ->
        val channel = view.channels.firstOrNull { it.id.toLong() == id }
        if (channel == null) {
            passwordFor = null
        } else {
            PasswordDialog(
                channelName = channel.name,
                onJoin = { password ->
                    passwordFor = null
                    onJoin(channel.id, password)
                },
                onDismiss = { passwordFor = null },
            )
        }
    }
}

@Composable
private fun ChannelRow(channel: ChannelView, mine: Boolean, onOpen: () -> Unit, onJoin: () -> Unit) {
    val icon: ImageVector = when {
        channel.private -> Icons.Outlined.VisibilityOff
        channel.hasPassword -> Icons.Outlined.Lock
        channel.encrypted -> Icons.Outlined.EnhancedEncryption
        else -> Icons.Outlined.Tag
    }
    Row(
        Modifier
            .fillMaxWidth()
            .clickable(onClick = onOpen)
            .background(if (mine) MaterialTheme.colorScheme.primaryContainer else Color.Transparent)
            .padding(start = (16 + INDENT * channel.depth.toInt()).dp, end = 4.dp, top = 2.dp, bottom = 2.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(icon, contentDescription = null, tint = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.size(18.dp))
        Spacer(Modifier.size(10.dp))
        Column(Modifier.weight(1f).padding(vertical = 8.dp)) {
            Text(
                channel.name,
                style = MaterialTheme.typography.bodyLarge,
                fontWeight = if (channel.unread > 0u) FontWeight.SemiBold else FontWeight.Normal,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            if (channel.topic.isNotBlank()) {
                Text(
                    channel.topic,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
        if (channel.unread > 0u) {
            Badge { Text(if (channel.unread > 99u) "99+" else channel.unread.toString()) }
            Spacer(Modifier.size(4.dp))
        }
        if (!mine) {
            IconButton(onClick = onJoin) {
                Icon(
                    Icons.AutoMirrored.Outlined.Login,
                    contentDescription = stringResource(R.string.server_join, channel.name),
                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        } else {
            Spacer(Modifier.size(48.dp))
        }
    }
}

@Composable
private fun ClientRow(client: ClientView, depth: Int) {
    Row(
        Modifier
            .fillMaxWidth()
            .padding(start = (16 + INDENT * depth).dp, end = 16.dp, top = 4.dp, bottom = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Avatar(client.nickname, talking = client.talking)
        Spacer(Modifier.size(10.dp))
        Text(
            if (client.isMe) stringResource(R.string.server_you, client.nickname) else client.nickname,
            style = MaterialTheme.typography.bodyMedium,
            fontWeight = if (client.isMe) FontWeight.Medium else FontWeight.Normal,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.weight(1f, fill = false),
        )
        if (client.platform == ClientPlatform.TEAM_SPEAK) {
            Spacer(Modifier.size(6.dp))
            Tag("TS")
        }
        client.away?.let {
            Spacer(Modifier.size(6.dp))
            Text(
                if (it.isBlank()) stringResource(R.string.server_away) else it,
                style = MaterialTheme.typography.bodySmall,
                color = GwarColors.warn,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        Spacer(Modifier.weight(1f))
        val muted = MaterialTheme.colorScheme.onSurfaceVariant
        if (client.muted) Icon(Icons.Outlined.MicOff, stringResource(R.string.server_muted), Modifier.size(16.dp), tint = muted)
        if (client.deafened) Icon(Icons.Outlined.HeadsetOff, stringResource(R.string.server_deafened), Modifier.size(16.dp), tint = muted)
    }
}

@Composable
fun Avatar(name: String, talking: Boolean, size: Int = 26) {
    val ring = if (talking) GwarColors.talking else Color.Transparent
    Box(
        Modifier
            .size(size.dp)
            .border(BorderStroke(2.dp, ring), CircleShape)
            .padding(2.dp)
            .background(MaterialTheme.colorScheme.surfaceContainerHighest, CircleShape),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            name.trim().take(1).uppercase(),
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
fun Tag(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.labelSmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier
            .background(MaterialTheme.colorScheme.surfaceContainerHighest, RoundedCornerShape(4.dp))
            .padding(horizontal = 5.dp, vertical = 1.dp),
    )
}

@Composable
private fun PasswordDialog(channelName: String, onJoin: (String) -> Unit, onDismiss: () -> Unit) {
    var password by rememberSaveable { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.server_password_title, channelName)) },
        text = {
            OutlinedTextField(
                value = password,
                onValueChange = { password = it },
                label = { Text(stringResource(R.string.server_password_label)) },
                singleLine = true,
                visualTransformation = PasswordVisualTransformation(),
            )
        },
        confirmButton = { TextButton(onClick = { onJoin(password) }) { Text(stringResource(R.string.server_password_join)) } },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.cancel)) } },
    )
}

/** Shows a notice once in the snackbar. */
@Composable
fun NoticeEffect(notice: Notice?, snackbar: SnackbarHostState, onShown: () -> Unit) {
    val text = notice?.let { describe(it) }
    LaunchedEffect(notice) {
        if (text != null) {
            onShown()
            snackbar.showSnackbar(text)
        }
    }
}


/** The channel I am in, the call's state and its controls. */
@Composable
private fun VoiceBar(
    channelName: String,
    voice: VoiceState?,
    muted: Boolean,
    deafened: Boolean,
    reconnecting: Boolean,
    output: AudioOutput?,
    outputs: List<AudioOutput>,
    onChooseOutput: (OutputKind) -> Unit,
    onAllowBluetooth: (() -> Unit)?,
    pushToTalk: Boolean,
    onSetPushToTalk: (Boolean) -> Unit,
    thresholdDb: Float,
    onSetThreshold: (Float) -> Unit,
    onToggleMute: () -> Unit,
    onToggleDeafen: () -> Unit,
    onLeave: () -> Unit,
) {
    Surface(color = MaterialTheme.colorScheme.surfaceContainerHigh) {
        Row(
            Modifier.fillMaxWidth().navigationBarsPadding().padding(start = 16.dp, end = 8.dp, top = 8.dp, bottom = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            // Tapping the status shows the engine's counters, for trying the app on real phones.
            var details by rememberSaveable { mutableStateOf(false) }
            Column(Modifier.weight(1f).clickable(enabled = voice?.status != null) { details = !details }) {
                Text(
                    channelName,
                    style = MaterialTheme.typography.bodyMedium,
                    fontWeight = FontWeight.Medium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                val link = voice?.status?.link
                val (text, color) = when {
                    reconnecting -> stringResource(R.string.server_reconnecting) to GwarColors.warn
                    voice == null -> stringResource(R.string.voice_off) to MaterialTheme.colorScheme.onSurfaceVariant
                    link == VoiceLink.CONNECTED -> stringResource(R.string.voice_connected) to GwarColors.talking
                    link == VoiceLink.LOST -> stringResource(R.string.voice_reconnecting) to GwarColors.warn
                    else -> stringResource(R.string.voice_connecting) to MaterialTheme.colorScheme.onSurfaceVariant
                }
                Row(verticalAlignment = Alignment.CenterVertically) {
                    if (voice?.status?.transmitting == true) {
                        Box(Modifier.size(8.dp).background(GwarColors.talking, CircleShape))
                        Spacer(Modifier.size(6.dp))
                    }
                    Text(text, style = MaterialTheme.typography.bodySmall, color = color, maxLines = 1)
                }
                val status = voice?.status
                if (details && status != null) {
                    Text(
                        stringResource(
                            R.string.voice_details,
                            status.framesSent.toLong(),
                            status.framesReceived.toLong(),
                            status.underruns.toLong(),
                            status.inputLevelDb.toInt(),
                        ),
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
            if (voice != null) VoiceOptions(pushToTalk, onSetPushToTalk, thresholdDb, onSetThreshold, voice)
            if (voice != null && output != null) {
                OutputMenu(output, outputs, onChooseOutput, onAllowBluetooth)
            }
            if (voice != null) {
                val off = MaterialTheme.colorScheme.error
                IconButton(onClick = onToggleMute) {
                    if (muted || deafened) {
                        Icon(Icons.Outlined.MicOff, stringResource(R.string.voice_unmute), tint = off)
                    } else {
                        Icon(Icons.Outlined.Mic, stringResource(R.string.voice_mute))
                    }
                }
                IconButton(onClick = onToggleDeafen) {
                    if (deafened) {
                        Icon(Icons.Outlined.HeadsetOff, stringResource(R.string.voice_undeafen), tint = off)
                    } else {
                        Icon(Icons.Outlined.Headset, stringResource(R.string.voice_deafen))
                    }
                }
            }
            FilledIconButton(
                onClick = onLeave,
                colors = IconButtonDefaults.filledIconButtonColors(
                    containerColor = MaterialTheme.colorScheme.error,
                    contentColor = MaterialTheme.colorScheme.onError,
                ),
            ) {
                Icon(Icons.Outlined.CallEnd, stringResource(R.string.server_leave))
            }
        }
    }
}

@Composable
private fun OutputMenu(
    output: AudioOutput,
    outputs: List<AudioOutput>,
    onChoose: (OutputKind) -> Unit,
    onAllowBluetooth: (() -> Unit)?,
) {
    var open by remember { mutableStateOf(false) }
    Box {
        IconButton(onClick = { open = true }) {
            Icon(output.kind.icon(), stringResource(R.string.voice_output, output.label()))
        }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            for (choice in outputs) {
                DropdownMenuItem(
                    text = { Text(choice.label()) },
                    leadingIcon = { Icon(choice.kind.icon(), null) },
                    trailingIcon = if (choice == output) {
                        { Icon(Icons.Outlined.Check, null) }
                    } else {
                        null
                    },
                    onClick = {
                        open = false
                        onChoose(choice.kind)
                    },
                )
            }
            if (onAllowBluetooth != null) {
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.voice_output_allow_bluetooth)) },
                    leadingIcon = { Icon(Icons.Outlined.Bluetooth, null) },
                    onClick = {
                        open = false
                        onAllowBluetooth()
                    },
                )
            }
        }
    }
}

private fun OutputKind.icon(): ImageVector = when (this) {
    OutputKind.SPEAKER -> Icons.AutoMirrored.Outlined.VolumeUp
    OutputKind.EARPIECE -> Icons.Outlined.PhoneInTalk
    OutputKind.WIRED -> Icons.Outlined.Headphones
    OutputKind.BLUETOOTH -> Icons.Outlined.Bluetooth
    OutputKind.USB -> Icons.Outlined.Usb
}

@Composable
private fun AudioOutput.label(): String = when (kind) {
    OutputKind.SPEAKER -> stringResource(R.string.voice_output_speaker)
    OutputKind.EARPIECE -> stringResource(R.string.voice_output_earpiece)
    OutputKind.WIRED -> stringResource(R.string.voice_output_wired)
    OutputKind.BLUETOOTH, OutputKind.USB -> name.ifBlank {
        stringResource(if (kind == OutputKind.USB) R.string.voice_output_usb else R.string.voice_output_bluetooth)
    }
}

@Composable
private fun VoiceOptions(
    pushToTalk: Boolean,
    onSetPushToTalk: (Boolean) -> Unit,
    thresholdDb: Float,
    onSetThreshold: (Float) -> Unit,
    voice: VoiceState,
) {
    var open by remember { mutableStateOf(false) }
    var sensitivity by remember { mutableStateOf(false) }
    Box {
        IconButton(onClick = { open = true }) {
            Icon(Icons.Outlined.MoreVert, stringResource(R.string.voice_more))
        }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            DropdownMenuItem(
                text = { Text(stringResource(R.string.voice_push_to_talk)) },
                trailingIcon = { Checkbox(checked = pushToTalk, onCheckedChange = null) },
                onClick = {
                    open = false
                    onSetPushToTalk(!pushToTalk)
                },
            )
            if (!pushToTalk) {
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.voice_sensitivity)) },
                    onClick = {
                        open = false
                        sensitivity = true
                    },
                )
            }
        }
    }
    if (sensitivity) SensitivityDialog(thresholdDb, onSetThreshold, voice) { sensitivity = false }
}

private const val METER_MIN_DB = -80f
private const val METER_MAX_DB = -10f

/** The voice activity threshold, over a live meter of the microphone level. */
@Composable
private fun SensitivityDialog(thresholdDb: Float, onSet: (Float) -> Unit, voice: VoiceState, onDismiss: () -> Unit) {
    val level = voice.status?.inputLevelDb ?: METER_MIN_DB
    val open = voice.status?.transmitting == true
    fun fraction(db: Float) = ((db - METER_MIN_DB) / (METER_MAX_DB - METER_MIN_DB)).coerceIn(0f, 1f)
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.voice_sensitivity)) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                Text(stringResource(R.string.voice_sensitivity_hint), style = MaterialTheme.typography.bodyMedium)
                Box(
                    Modifier
                        .fillMaxWidth()
                        .height(10.dp)
                        .background(MaterialTheme.colorScheme.surfaceContainerHighest, RoundedCornerShape(5.dp)),
                ) {
                    Box(
                        Modifier
                            .fillMaxWidth(fraction(level))
                            .height(10.dp)
                            .background(
                                if (open) GwarColors.talking else MaterialTheme.colorScheme.onSurfaceVariant,
                                RoundedCornerShape(5.dp),
                            ),
                    )
                }
                Slider(
                    value = thresholdDb,
                    onValueChange = onSet,
                    valueRange = METER_MIN_DB..METER_MAX_DB,
                )
                Text(
                    stringResource(R.string.voice_sensitivity_value, thresholdDb.toInt()),
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.done)) } },
        dismissButton = {
            TextButton(onClick = { onSet(DEFAULT_THRESHOLD_DB) }) { Text(stringResource(R.string.voice_sensitivity_default)) }
        },
    )
}

/** Held to open the microphone in push-to-talk mode. */
@Composable
private fun TalkButton(held: Boolean, onHold: (Boolean) -> Unit) {
    val haptics = LocalHapticFeedback.current
    val color = if (held) GwarColors.talking else MaterialTheme.colorScheme.primary
    // Dark text on the light talking green, white on the accent.
    val content = if (held) Color(0xFF0E0F12) else MaterialTheme.colorScheme.onPrimary
    Surface(color = MaterialTheme.colorScheme.surfaceContainerHigh) {
        Box(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 10.dp), contentAlignment = Alignment.Center) {
            Box(
                Modifier
                    .fillMaxWidth()
                    .height(64.dp)
                    .background(color, RoundedCornerShape(32.dp))
                    .pointerInput(Unit) {
                        detectTapGestures(
                            onPress = {
                                haptics.performHapticFeedback(HapticFeedbackType.LongPress)
                                onHold(true)
                                try {
                                    tryAwaitRelease()
                                } finally {
                                    onHold(false)
                                }
                            },
                        )
                    }
                    .semantics { role = Role.Button },
                contentAlignment = Alignment.Center,
            ) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(Icons.Outlined.Mic, null, tint = content)
                    Spacer(Modifier.size(8.dp))
                    Text(
                        stringResource(if (held) R.string.voice_talking else R.string.voice_hold_to_talk),
                        color = content,
                        style = MaterialTheme.typography.titleMedium,
                    )
                }
            }
        }
    }
}

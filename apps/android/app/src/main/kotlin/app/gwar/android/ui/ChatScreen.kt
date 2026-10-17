package app.gwar.android.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.outlined.AttachFile
import androidx.compose.material.icons.outlined.EnhancedEncryption
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import app.gwar.android.ChatState
import app.gwar.android.R
import app.gwar.core.AppException
import app.gwar.core.ChannelView
import app.gwar.core.ChatLine
import app.gwar.core.Verification
import java.text.DateFormat
import java.util.Date

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChatScreen(
    channel: ChannelView,
    chat: ChatState?,
    notice: AppException?,
    onBack: () -> Unit,
    onSend: (String) -> Unit,
    onLoadOlder: () -> Unit,
    onDismissNotice: () -> Unit,
) {
    val snackbar = remember { SnackbarHostState() }
    NoticeEffect(notice, snackbar, onDismissNotice)
    var draft by rememberSaveable(channel.id.toLong()) { mutableStateOf("") }

    Scaffold(
        topBar = {
            TopAppBar(
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = stringResource(R.string.back))
                    }
                },
                title = {
                    Column {
                        Text(channel.name, maxLines = 1, overflow = TextOverflow.Ellipsis, fontWeight = FontWeight.SemiBold)
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
                },
            )
        },
        bottomBar = {
            if (!channel.encrypted) {
                Surface(color = MaterialTheme.colorScheme.surfaceContainer) {
                    Row(
                        Modifier.fillMaxWidth().navigationBarsPadding().imePadding().padding(8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        OutlinedTextField(
                            value = draft,
                            onValueChange = { draft = it },
                            placeholder = { Text(stringResource(R.string.chat_placeholder, channel.name)) },
                            maxLines = 5,
                            modifier = Modifier.weight(1f),
                        )
                        IconButton(
                            onClick = {
                                onSend(draft)
                                draft = ""
                            },
                            enabled = draft.isNotBlank(),
                        ) {
                            Icon(Icons.AutoMirrored.Filled.Send, contentDescription = stringResource(R.string.chat_send))
                        }
                    }
                }
            }
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        Box(Modifier.fillMaxSize().padding(padding)) {
            when {
                channel.encrypted -> Placeholder(stringResource(R.string.chat_encrypted_unsupported), encrypted = true)
                chat == null || (chat.lines.isEmpty() && chat.loading) -> CircularProgressIndicator(Modifier.align(Alignment.Center))
                chat.lines.isEmpty() -> Placeholder(stringResource(R.string.chat_empty), encrypted = false)
                else -> Messages(chat, onLoadOlder)
            }
        }
    }
}

@Composable
private fun Messages(chat: ChatState, onLoadOlder: () -> Unit) {
    val list = rememberLazyListState()
    val newestFirst = chat.lines.asReversed()
    // Reversed layout: index 0 is the newest message, at the bottom.
    val nearOldest by remember {
        derivedStateOf { (list.layoutInfo.visibleItemsInfo.lastOrNull()?.index ?: 0) >= list.layoutInfo.totalItemsCount - 3 }
    }
    LaunchedEffect(nearOldest, chat.lines.size) {
        if (nearOldest && !chat.complete && !chat.loading) onLoadOlder()
    }
    LaunchedEffect(chat.lines.lastOrNull()?.id) {
        if (list.firstVisibleItemIndex <= 1) list.animateScrollToItem(0)
    }
    LazyColumn(
        state = list,
        reverseLayout = true,
        verticalArrangement = Arrangement.spacedBy(2.dp, Alignment.Bottom),
        modifier = Modifier.fillMaxSize(),
    ) {
        items(newestFirst, key = { it.id.toLong() }) { line -> Message(line) }
        if (chat.loading) {
            item(key = "loading") {
                Box(Modifier.fillMaxWidth().padding(12.dp), contentAlignment = Alignment.Center) {
                    CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
                }
            }
        }
    }
}

private val timeFormat: DateFormat = DateFormat.getTimeInstance(DateFormat.SHORT)

@Composable
private fun Message(line: ChatLine) {
    Row(Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 6.dp)) {
        Avatar(line.authorName, talking = false, size = 32)
        Spacer(Modifier.size(10.dp))
        Column(Modifier.weight(1f)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    line.authorName,
                    style = MaterialTheme.typography.bodyMedium,
                    fontWeight = FontWeight.SemiBold,
                    color = if (line.own) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.onSurface,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f, fill = false),
                )
                if (line.verification == Verification.TEAM_SPEAK) {
                    Spacer(Modifier.size(6.dp))
                    Tag("TS")
                }
                Spacer(Modifier.size(8.dp))
                Text(
                    timeFormat.format(Date(line.sentAt)),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Text(line.text, style = MaterialTheme.typography.bodyMedium)
            if (line.edited) {
                Text(
                    stringResource(R.string.chat_edited),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            for (name in line.attachments) {
                Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.padding(top = 2.dp)) {
                    Icon(Icons.Outlined.AttachFile, null, Modifier.size(14.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
                    Text(
                        name,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
        }
    }
}

@Composable
private fun Placeholder(text: String, encrypted: Boolean) {
    Column(
        Modifier.fillMaxSize().padding(32.dp),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        if (encrypted) {
            Icon(Icons.Outlined.EnhancedEncryption, null, Modifier.size(32.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
            Spacer(Modifier.size(12.dp))
        }
        Text(
            text,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
    }
}

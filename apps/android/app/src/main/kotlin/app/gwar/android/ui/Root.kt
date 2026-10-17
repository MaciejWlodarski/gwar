package app.gwar.android.ui

import androidx.activity.compose.BackHandler
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import app.gwar.android.GwarViewModel

@Composable
fun GwarRoot(model: GwarViewModel) {
    val state by model.state.collectAsStateWithLifecycle()
    val view = state.view
    if (view == null) {
        ConnectScreen(
            state = state,
            onForm = model::updateForm,
            onConnect = model::connect,
            onTrustNewKey = model::trustNewKey,
            onDismissProblem = model::dismissProblem,
        )
        return
    }
    val open = state.openChannel?.let { id -> view.channels.firstOrNull { it.id == id } }
    if (open != null) {
        BackHandler(onBack = model::closeChat)
        ChatScreen(
            channel = open,
            chat = state.chats[open.id],
            notice = state.notice,
            onBack = model::closeChat,
            onSend = { text -> model.send(open.id, text) },
            onLoadOlder = { model.loadOlder(open.id) },
            onDismissNotice = model::dismissNotice,
        )
    } else {
        ServerScreen(
            view = view,
            notice = state.notice,
            onOpenChat = model::openChat,
            onJoin = model::joinChannel,
            onLeave = model::leaveChannel,
            onDisconnect = model::disconnect,
            onDismissNotice = model::dismissNotice,
        )
    }
}

package app.gwar.android

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import app.gwar.core.AppException
import app.gwar.core.ChatLine
import app.gwar.core.CloseReason
import app.gwar.core.ConnectRequest
import app.gwar.core.ServerView
import app.gwar.core.Session
import app.gwar.core.SessionListener
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch

data class ConnectForm(
    val address: String = "",
    val nickname: String = "",
    val insecureLan: Boolean = false,
    val serverPassword: String = "",
)

/** Why the connect screen shows a message. */
sealed interface Problem {
    data class Failed(val error: AppException) : Problem

    /** The server's key is not the one pinned for this address. */
    data class IdentityChanged(val endpoint: String) : Problem

    data class Closed(val reason: CloseReason) : Problem
}

/** One channel's chat as loaded so far, oldest first. */
data class ChatState(
    val lines: List<ChatLine> = emptyList(),
    val loading: Boolean = false,
    /** The oldest message is loaded. */
    val complete: Boolean = false,
)

data class UiState(
    val uid: String,
    val form: ConnectForm,
    val connecting: Boolean = false,
    val problem: Problem? = null,
    /** Set while connected. */
    val view: ServerView? = null,
    val openChannel: UInt? = null,
    val chats: Map<UInt, ChatState> = emptyMap(),
    /** A failed action while connected, shown once. */
    val notice: AppException? = null,
)

private const val HISTORY_PAGE = 50

class GwarViewModel(application: Application) : AndroidViewModel(application) {
    private val core = (application as GwarApplication).core
    private val _state = MutableStateFlow(
        UiState(
            uid = core.uid(),
            form = core.lastServer()?.let { ConnectForm(it.address, it.nickname, it.insecureLan) } ?: ConnectForm(),
        ),
    )
    val state: StateFlow<UiState> = _state.asStateFlow()

    // Native memory is freed by the bindings' cleaner once unreachable; no explicit
    // destroy(), since an action still in flight may hold the session.
    @Volatile
    private var session: Session? = null

    /** Bumped on every connect and disconnect, so a closed session's late updates are ignored. */
    @Volatile
    private var generation = 0

    fun updateForm(form: ConnectForm) = _state.update { it.copy(form = form) }

    fun dismissProblem() = _state.update { it.copy(problem = null) }

    fun dismissNotice() = _state.update { it.copy(notice = null) }

    fun connect() {
        val current = _state.value
        if (current.connecting || session != null) return
        val form = current.form
        _state.update { it.copy(connecting = true, problem = null) }
        val listener = Listener(++generation)
        viewModelScope.launch {
            try {
                val request = ConnectRequest(
                    address = form.address,
                    nickname = form.nickname.trim(),
                    insecureLan = form.insecureLan,
                    serverPassword = form.serverPassword.ifEmpty { null },
                )
                val connected = core.connect(request, listener)
                session = connected
                _state.update {
                    it.copy(connecting = false, view = connected.view(), openChannel = null, chats = emptyMap())
                }
            } catch (e: AppException.ServerIdentityChanged) {
                _state.update { it.copy(connecting = false, problem = Problem.IdentityChanged(e.endpoint)) }
            } catch (e: AppException) {
                _state.update { it.copy(connecting = false, problem = Problem.Failed(e)) }
            }
        }
    }

    /** Accepts the server's new key after the user confirmed, then connects again. */
    fun trustNewKey(endpoint: String) {
        try {
            core.forgetServer(endpoint)
        } catch (e: AppException) {
            _state.update { it.copy(problem = Problem.Failed(e)) }
            return
        }
        connect()
    }

    fun disconnect() {
        generation++
        session?.disconnect()
        session = null
        _state.update { it.copy(view = null, openChannel = null, chats = emptyMap(), notice = null) }
    }

    fun joinChannel(channel: UInt, password: String? = null) = act { it.joinChannel(channel, password) }

    fun leaveChannel() = act { it.leaveChannel() }

    fun openChat(channel: UInt) {
        _state.update { it.copy(openChannel = channel) }
        session?.setOpenChannel(channel)
        if (_state.value.chats[channel] == null) loadHistory(channel, older = false)
    }

    fun closeChat() {
        _state.update { it.copy(openChannel = null) }
        session?.setOpenChannel(null)
    }

    fun loadOlder(channel: UInt) {
        val chat = _state.value.chats[channel] ?: return
        if (chat.loading || chat.complete) return
        loadHistory(channel, older = true)
    }

    fun send(channel: UInt, text: String) {
        val body = text.trim()
        if (body.isEmpty()) return
        act { session ->
            val line = session.sendMessage(channel, body)
            _state.update { it.copy(chats = it.chats.merge(line)) }
        }
    }

    private fun loadHistory(channel: UInt, older: Boolean) {
        val s = session ?: return
        val before = if (older) _state.value.chats[channel]?.lines?.firstOrNull()?.id else null
        _state.update { it.copy(chats = it.chats + (channel to (it.chats[channel] ?: ChatState()).copy(loading = true))) }
        viewModelScope.launch {
            try {
                val page = s.history(channel, before)
                _state.update {
                    val chat = it.chats[channel] ?: ChatState()
                    val lines = (page + chat.lines).associateBy { line -> line.id }.values.sortedBy { line -> line.id }
                    it.copy(chats = it.chats + (channel to ChatState(lines, loading = false, complete = page.size < HISTORY_PAGE)))
                }
            } catch (e: AppException) {
                _state.update {
                    val chat = (it.chats[channel] ?: ChatState()).copy(loading = false)
                    it.copy(chats = it.chats + (channel to chat), notice = e)
                }
            }
        }
    }

    private fun act(block: suspend (Session) -> Unit) {
        val s = session ?: return
        viewModelScope.launch {
            try {
                block(s)
            } catch (e: AppException) {
                _state.update { it.copy(notice = e) }
            }
        }
    }

    override fun onCleared() {
        disconnect()
    }

    private inner class Listener(private val id: Int) : SessionListener {
        private val current get() = id == generation

        override fun onView(view: ServerView) {
            if (current) _state.update { it.copy(view = view) }
        }

        override fun onChat(line: ChatLine) {
            if (current) _state.update { it.copy(chats = it.chats.merge(line)) }
        }

        override fun onChatDeleted(channel: UInt, message: UInt) {
            if (!current) return
            _state.update {
                val chat = it.chats[channel] ?: return@update it
                it.copy(chats = it.chats + (channel to chat.copy(lines = chat.lines.filter { l -> l.id != message })))
            }
        }

        override fun onClosed(reason: CloseReason) {
            if (!current) return
            generation++
            session = null
            _state.update {
                it.copy(view = null, openChannel = null, chats = emptyMap(), notice = null, problem = Problem.Closed(reason))
            }
        }
    }
}

/** Adds or replaces (an edit) a line in a chat that is loaded; others load it with their history. */
private fun Map<UInt, ChatState>.merge(line: ChatLine): Map<UInt, ChatState> {
    val chat = this[line.channel] ?: return this
    val lines = (chat.lines.filter { it.id != line.id } + line).sortedBy { it.id }
    return this + (line.channel to chat.copy(lines = lines))
}

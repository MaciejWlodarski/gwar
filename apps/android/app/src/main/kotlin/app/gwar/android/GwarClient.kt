package app.gwar.android

import android.app.Application
import app.gwar.core.AppException
import app.gwar.core.ChatLine
import app.gwar.core.CloseReason
import app.gwar.core.ConnectRequest
import app.gwar.core.ServerView
import app.gwar.core.Session
import app.gwar.core.SessionListener
import app.gwar.core.VoiceLink
import app.gwar.core.VoiceStatus
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.isActive
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

/** Voice in the channel I am in. */
data class VoiceState(
    /** `null` until the engine runs. */
    val status: VoiceStatus? = null,
    val muted: Boolean = false,
    val deafened: Boolean = false,
)

/** Something to tell the user once, while connected. */
sealed interface Notice {
    data class Failed(val error: AppException) : Notice

    /** Joined without voice: the microphone permission was refused. */
    data object MicrophoneNeeded : Notice
}

data class UiState(
    val uid: String,
    val form: ConnectForm,
    val connecting: Boolean = false,
    val problem: Problem? = null,
    /** Set while connected. */
    val view: ServerView? = null,
    val openChannel: UInt? = null,
    val chats: Map<UInt, ChatState> = emptyMap(),
    /** Set while voice is wanted: starting, running or reconnecting. */
    val voice: VoiceState? = null,
    val notice: Notice? = null,
)

private const val HISTORY_PAGE = 50
private const val STATUS_INTERVAL_MS = 250L
private const val RECONNECT_DELAY_MS = 1_500L

/**
 * The app's one connection, owned by the process rather than a screen, so a
 * call keeps going while the screen is off or the app is in the background
 * ([VoiceService] keeps the process alive meanwhile). Runs on the main thread;
 * the core's callbacks only update [state].
 */
class GwarClient(private val app: Application) {
    private val core = (app as GwarApplication).core
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val audio = CallAudio(app)

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

    private var voiceJob: Job? = null

    fun updateForm(form: ConnectForm) = _state.update { it.copy(form = form) }

    fun dismissProblem() = _state.update { it.copy(problem = null) }

    fun dismissNotice() = _state.update { it.copy(notice = null) }

    fun connect() {
        val current = _state.value
        if (current.connecting || session != null) return
        val form = current.form
        _state.update { it.copy(connecting = true, problem = null) }
        val listener = Listener(++generation)
        scope.launch {
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
        endVoice()
        session?.disconnect()
        session = null
        _state.update { it.copy(view = null, openChannel = null, chats = emptyMap(), notice = null) }
    }

    /** Joins a channel, with voice unless [withVoice] is false (no microphone permission). */
    fun joinChannel(channel: UInt, password: String? = null, withVoice: Boolean = true) {
        val s = session ?: return
        scope.launch {
            try {
                endVoice()
                s.joinChannel(channel, password)
                when {
                    !withVoice -> _state.update { it.copy(notice = Notice.MicrophoneNeeded) }
                    else -> startVoice(s, channel)
                }
            } catch (e: AppException) {
                _state.update { it.copy(notice = Notice.Failed(e)) }
            }
        }
    }

    fun leaveChannel() {
        endVoice()
        act { it.leaveChannel() }
    }

    fun toggleMute() {
        val voice = _state.value.voice ?: return
        // Unmuting while deafened also undeafens; the core does the same.
        val muted = if (voice.deafened) false else !voice.muted
        _state.update { it.copy(voice = it.voice?.copy(muted = muted, deafened = if (muted) voice.deafened else false)) }
        act { it.setMuted(muted) }
    }

    fun toggleDeafen() {
        val voice = _state.value.voice ?: return
        val deafened = !voice.deafened
        _state.update { it.copy(voice = it.voice?.copy(deafened = deafened)) }
        act { it.setDeafened(deafened) }
    }

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

    /**
     * Starts the engine and watches it: a lost link (the server moved me, the
     * network changed, a headset was unplugged) starts it again while I am
     * still in a channel.
     */
    private suspend fun startVoice(s: Session, channelId: UInt) {
        val channel = s.view().channels.firstOrNull { it.id == channelId }
        // Refused up front: the service must not be stopped before it reaches the foreground.
        if (channel?.encrypted == true) {
            _state.update { it.copy(notice = Notice.Failed(AppException.EncryptedVoice())) }
            return
        }
        val muted = s.muted()
        val deafened = s.deafened()
        _state.update { it.copy(voice = VoiceState(muted = muted, deafened = deafened)) }
        audio.enter()
        VoiceService.start(app, channel?.name.orEmpty())
        try {
            s.startVoice()
        } catch (e: AppException) {
            endVoice()
            _state.update { it.copy(notice = Notice.Failed(e)) }
            return
        }
        voiceJob = scope.launch {
            while (isActive) {
                delay(STATUS_INTERVAL_MS)
                val status = s.voiceStatus()
                _state.update { it.copy(voice = it.voice?.copy(status = status)) }
                if (status?.link == VoiceLink.LOST && _state.value.view?.myChannel != null) {
                    delay(RECONNECT_DELAY_MS)
                    try {
                        s.startVoice()
                    } catch (e: AppException) {
                        // Keep trying while the channel is joined; the status shows the link as lost.
                    }
                }
            }
        }
    }

    /** Stops the engine, call audio and the service. */
    private fun endVoice() {
        voiceJob?.cancel()
        voiceJob = null
        if (_state.value.voice == null) return
        session?.stopVoice()
        audio.leave()
        VoiceService.stop(app)
        _state.update { it.copy(voice = null) }
    }

    private fun loadHistory(channel: UInt, older: Boolean) {
        val s = session ?: return
        val before = if (older) _state.value.chats[channel]?.lines?.firstOrNull()?.id else null
        _state.update { it.copy(chats = it.chats + (channel to (it.chats[channel] ?: ChatState()).copy(loading = true))) }
        scope.launch {
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
                    it.copy(chats = it.chats + (channel to chat), notice = Notice.Failed(e))
                }
            }
        }
    }

    private fun act(block: suspend (Session) -> Unit) {
        val s = session ?: return
        scope.launch {
            try {
                block(s)
            } catch (e: AppException) {
                _state.update { it.copy(notice = Notice.Failed(e)) }
            }
        }
    }

    private inner class Listener(private val id: Int) : SessionListener {
        private val current get() = id == generation

        override fun onView(view: ServerView) {
            if (!current) return
            _state.update { it.copy(view = view) }
            // Taken out of every channel (moved to none, kicked from the channel): no voice.
            if (view.myChannel == null) scope.launch { if (current) endVoice() }
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
            scope.launch {
                if (!current) return@launch
                generation++
                endVoice()
                // A dead link may still hold its socket; let it go.
                session?.disconnect()
                session = null
                _state.update {
                    it.copy(view = null, openChannel = null, chats = emptyMap(), notice = null, problem = Problem.Closed(reason))
                }
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

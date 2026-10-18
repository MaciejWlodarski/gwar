package app.gwar.android

import android.Manifest
import android.app.Application
import android.content.Context
import android.content.pm.PackageManager
import app.gwar.core.AppException
import app.gwar.core.ChatLine
import app.gwar.core.CloseReason
import app.gwar.core.ConnectRequest
import app.gwar.core.ServerView
import app.gwar.core.Session
import app.gwar.core.SessionListener
import app.gwar.core.VoiceLink
import app.gwar.core.VoiceStatus
import app.gwar.core.reconnectDelayMs
import app.gwar.core.shouldReconnect
import app.gwar.core.shouldRetry
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
    /** `null` until the engine runs, and while the session reconnects. */
    val status: VoiceStatus? = null,
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
    /** Set while connected, and kept (as last seen) while reconnecting. */
    val view: ServerView? = null,
    /** The link went away and the app is connecting again by itself. */
    val reconnecting: Boolean = false,
    val openChannel: UInt? = null,
    val chats: Map<UInt, ChatState> = emptyMap(),
    /** Set while voice is wanted: starting, running or reconnecting. */
    val voice: VoiceState? = null,
    /** Kept across channels and reconnections, as the other apps do. */
    val muted: Boolean = false,
    val deafened: Boolean = false,
    /** The microphone opens only while the talk button is held (a saved preference). */
    val pushToTalk: Boolean = false,
    val talkHeld: Boolean = false,
    /** Voice activity opens the microphone above this level, in dBFS (a saved preference). */
    val thresholdDb: Float = DEFAULT_THRESHOLD_DB,
    /** Where call audio can go while voice runs, and where it goes. */
    val outputs: List<AudioOutput> = emptyList(),
    val output: AudioOutput? = null,
    val notice: Notice? = null,
)

private const val HISTORY_PAGE = 50
private const val STATUS_INTERVAL_MS = 250L
private const val VOICE_RETRY_MS = 1_500L
private const val PUSH_TO_TALK = "push_to_talk"
private const val THRESHOLD_DB = "threshold_db"

/** As on the desktop; the core clamps to -80..-10. */
const val DEFAULT_THRESHOLD_DB = -50f

/**
 * The app's one connection, owned by the process rather than a screen, so a
 * call keeps going while the screen is off or the app is in the background
 * ([VoiceService] keeps the process alive meanwhile). Runs on the main thread;
 * the core's callbacks only update [state].
 *
 * When the link drops (network change, ping timeout, server restart) it
 * connects again by itself and puts back what the user had: the channel, voice,
 * mute and deafen, the open chat. The call's service and audio mode stay up
 * meanwhile, so the process is not stopped in the background.
 */
class GwarClient(private val app: Application) {
    private val core = (app as GwarApplication).core()
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val audio = CallAudio(app) { outputs, output -> _state.update { it.copy(outputs = outputs, output = output) } }
    private val network = NetworkWatch(app) { onNetworkChanged() }

    private val settings = app.getSharedPreferences("settings", Context.MODE_PRIVATE)

    private val _state = MutableStateFlow(
        UiState(
            uid = core.uid(),
            form = core.lastServer()?.let { ConnectForm(it.address, it.nickname, it.insecureLan) } ?: ConnectForm(),
            pushToTalk = settings.getBoolean(PUSH_TO_TALK, false),
            thresholdDb = settings.getFloat(THRESHOLD_DB, DEFAULT_THRESHOLD_DB),
        ),
    )
    val state: StateFlow<UiState> = _state.asStateFlow()

    // Native memory is freed by the bindings' cleaner once unreachable; no explicit
    // destroy(), since an action still in flight may hold the session.
    @Volatile
    private var session: Session? = null

    /** Bumped for every new session and every teardown, so an old session's late updates are ignored. */
    @Volatile
    private var generation = 0

    /** How to connect again; `null` once the user disconnected. */
    private var request: ConnectRequest? = null

    /** What to put back after reconnecting. */
    private var wantedChannel: UInt? = null
    private var channelPassword: String? = null
    private var wantVoice = false

    /** A join or a restore is moving me; the views in between show the old channel. */
    private var moving = 0

    private var serviceRunning = false
    private var voiceJob: Job? = null
    private var reconnectJob: Job? = null

    fun updateForm(form: ConnectForm) = _state.update { it.copy(form = form) }

    fun dismissProblem() = _state.update { it.copy(problem = null) }

    fun dismissNotice() = _state.update { it.copy(notice = null) }

    fun connect() {
        val current = _state.value
        if (current.connecting || session != null || reconnectJob != null) return
        val form = current.form
        val request = ConnectRequest(
            address = form.address,
            nickname = form.nickname.trim(),
            insecureLan = form.insecureLan,
            serverPassword = form.serverPassword.ifEmpty { null },
        )
        _state.update { it.copy(connecting = true, problem = null) }
        val listener = Listener(++generation)
        scope.launch {
            try {
                val connected = core.connect(request, listener)
                this@GwarClient.request = request
                attach(connected)
                _state.update { it.copy(openChannel = null, chats = emptyMap()) }
            } catch (e: AppException) {
                _state.update { it.copy(connecting = false, problem = problemFor(e)) }
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

    fun disconnect() = teardown(problem = null)

    /** Joins a channel, with voice unless [withVoice] is false (no microphone permission). */
    fun joinChannel(channel: UInt, password: String? = null, withVoice: Boolean = true) {
        val s = session ?: return notConnected()
        scope.launch {
            moving++
            try {
                endVoice()
                s.joinChannel(channel, password)
                wantedChannel = channel
                channelPassword = password
                if (withVoice) startVoice(s, channel) else _state.update { it.copy(notice = Notice.MicrophoneNeeded) }
            } catch (e: AppException) {
                _state.update { it.copy(notice = Notice.Failed(e)) }
            } finally {
                moving--
            }
        }
    }

    fun leaveChannel() {
        wantedChannel = null
        channelPassword = null
        endVoice()
        act { it.leaveChannel() }
    }

    fun toggleMute() {
        val current = _state.value
        // Unmuting while deafened also undeafens; the core does the same.
        val muted = if (current.deafened) false else !current.muted
        _state.update { it.copy(muted = muted, deafened = if (muted) it.deafened else false) }
        act { it.setMuted(muted) }
    }

    fun toggleDeafen() {
        val deafened = !_state.value.deafened
        _state.update { it.copy(deafened = deafened) }
        act { it.setDeafened(deafened) }
    }

    fun chooseOutput(kind: OutputKind) = audio.choose(kind)

    fun setPushToTalk(enabled: Boolean) {
        settings.edit().putBoolean(PUSH_TO_TALK, enabled).apply()
        _state.update { it.copy(pushToTalk = enabled, talkHeld = false) }
        session?.setPushToTalk(enabled)
    }

    fun setThreshold(db: Float) {
        settings.edit().putFloat(THRESHOLD_DB, db).apply()
        _state.update { it.copy(thresholdDb = db) }
        session?.setVoiceActivityThreshold(db)
    }

    /** The talk button is pressed or released. */
    fun holdToTalk(held: Boolean) {
        _state.update { it.copy(talkHeld = held) }
        session?.setTalkHeld(held)
    }

    /** A permission changed (Bluetooth): list the outputs again. */
    fun refreshOutputs() = audio.refresh()

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

    private fun attach(s: Session) {
        session = s
        s.setPushToTalk(_state.value.pushToTalk)
        s.setVoiceActivityThreshold(_state.value.thresholdDb)
        network.start()
        _state.update { it.copy(connecting = false, reconnecting = false, view = s.view()) }
    }

    /** Ends everything: the session, voice, reconnection. Shows [problem] on the connect screen. */
    private fun teardown(problem: Problem?) {
        generation++
        reconnectJob?.cancel()
        reconnectJob = null
        network.stop()
        request = null
        wantedChannel = null
        channelPassword = null
        endVoice()
        session?.disconnect()
        session = null
        _state.update {
            it.copy(
                view = null, reconnecting = false, connecting = false, openChannel = null, chats = emptyMap(),
                notice = null, problem = problem,
            )
        }
    }

    /** Drops the current link (if any) and connects again, from a short first delay. */
    private fun reconnect() {
        if (request == null) return
        generation++
        session?.let {
            it.stopVoice()
            it.disconnect()
        }
        session = null
        voiceJob?.cancel()
        voiceJob = null
        _state.update { it.copy(reconnecting = true, voice = it.voice?.let { VoiceState() }) }
        reconnectJob?.cancel()
        reconnectJob = scope.launch {
            var attempt = 0u
            while (true) {
                delay(reconnectDelayMs(attempt).toLong())
                val request = request ?: return@launch
                try {
                    val connected = core.connect(request, Listener(++generation))
                    reconnectJob = null
                    attach(connected)
                    restore(connected)
                    return@launch
                } catch (e: AppException) {
                    if (!shouldRetry(e)) {
                        reconnectJob = null
                        teardown(problemFor(e))
                        return@launch
                    }
                }
                attempt++
            }
        }
    }

    /** Puts back what the user had before the link dropped. */
    private suspend fun restore(s: Session) {
        val channel = wantedChannel
        val password = channelPassword
        val voice = wantVoice
        moving++
        try {
            val current = _state.value
            if (current.muted) s.setMuted(true)
            if (current.deafened) s.setDeafened(true)
            if (channel != null && s.view().channels.any { it.id == channel }) {
                s.joinChannel(channel, password)
                if (voice && microphoneAllowed()) startVoice(s, channel) else endVoice()
            } else {
                wantedChannel = null
                endVoice()
            }
            _state.value.openChannel?.let { open ->
                s.setOpenChannel(open)
                loadHistory(open, older = false)
            }
        } catch (e: AppException) {
            _state.update { it.copy(notice = Notice.Failed(e)) }
        } finally {
            moving--
        }
    }

    private fun onNetworkChanged() {
        // The link was made on the network that went away; connect on the new one now.
        if (session != null || reconnectJob != null) reconnect()
    }

    /**
     * Starts the engine and watches it: a lost voice link (the server moved
     * me, a headset was unplugged) starts it again while the session lives.
     */
    private suspend fun startVoice(s: Session, channelId: UInt) {
        val channel = s.view().channels.firstOrNull { it.id == channelId }
        // Refused up front: the service must not be stopped before it reaches the foreground.
        if (channel?.encrypted == true) {
            _state.update { it.copy(notice = Notice.Failed(AppException.EncryptedVoice())) }
            return
        }
        wantVoice = true
        _state.update { it.copy(voice = VoiceState()) }
        audio.enter()
        if (!serviceRunning) {
            VoiceService.start(app, channel?.name.orEmpty())
            serviceRunning = true
        }
        try {
            s.startVoice()
        } catch (e: AppException) {
            endVoice()
            _state.update { it.copy(notice = Notice.Failed(e)) }
            return
        }
        voiceJob?.cancel()
        voiceJob = scope.launch {
            while (true) {
                delay(STATUS_INTERVAL_MS)
                if (session !== s) return@launch
                val status = s.voiceStatus()
                _state.update { it.copy(voice = it.voice?.copy(status = status)) }
                if (status?.link == VoiceLink.LOST) {
                    delay(VOICE_RETRY_MS)
                    try {
                        if (session === s) s.startVoice()
                    } catch (e: AppException) {
                        // Keep trying while the session lives; the status shows the link as lost.
                    }
                }
            }
        }
    }

    /** Stops the engine, call audio and the service: the user no longer wants voice. */
    private fun endVoice() {
        wantVoice = false
        voiceJob?.cancel()
        voiceJob = null
        session?.stopVoice()
        audio.leave()
        if (serviceRunning) {
            VoiceService.stop(app)
            serviceRunning = false
        }
        _state.update { it.copy(voice = null) }
    }

    private fun microphoneAllowed() =
        app.checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED

    private fun problemFor(e: AppException): Problem = when (e) {
        is AppException.ServerIdentityChanged -> Problem.IdentityChanged(e.endpoint)
        else -> Problem.Failed(e)
    }

    private fun notConnected() = _state.update { it.copy(notice = Notice.Failed(AppException.NotConnected())) }

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
                    // A reload after reconnecting keeps what older pages were loaded.
                    val complete = if (older) page.size < HISTORY_PAGE else chat.complete || page.size < HISTORY_PAGE
                    it.copy(chats = it.chats + (channel to ChatState(lines, loading = false, complete = complete)))
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
        val s = session ?: return notConnected()
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
            scope.launch {
                // The server moved me (or out of every channel); a join in flight moves me itself.
                if (!current || moving > 0 || view.myChannel == wantedChannel) return@launch
                wantedChannel = view.myChannel
                if (view.myChannel == null) endVoice()
            }
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
                if (request != null && shouldReconnect(reason)) reconnect() else teardown(Problem.Closed(reason))
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

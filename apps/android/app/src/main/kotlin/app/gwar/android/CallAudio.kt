package app.gwar.android

import android.content.Context
import android.media.AudioAttributes
import android.media.AudioDeviceCallback
import android.media.AudioDeviceInfo
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.os.Build
import android.os.Handler
import android.os.Looper

/** Where call audio can go. */
enum class OutputKind { SPEAKER, EARPIECE, WIRED, BLUETOOTH, USB }

data class AudioOutput(val kind: OutputKind, val name: String) {
    /** The platform device, from Android 12; earlier ones route by kind. */
    internal var device: AudioDeviceInfo? = null
}

/**
 * The phone's audio set up for a call while voice runs: communication mode
 * (so the platform's echo cancellation pairs the microphone with what the
 * speaker plays), call audio focus, and routing. By default a connected
 * headset, else the loudspeaker, as voice chat apps do (a phone call would
 * pick the earpiece); the user may pick another output, kept while it exists.
 */
class CallAudio(context: Context, private val onOutputs: (List<AudioOutput>, AudioOutput?) -> Unit) {
    private val manager = context.getSystemService(AudioManager::class.java)
    private val handler = Handler(Looper.getMainLooper())
    private var focus: AudioFocusRequest? = null
    private var active = false

    /** The user's pick; `null` routes automatically. */
    private var chosen: OutputKind? = null

    private val devices = object : AudioDeviceCallback() {
        override fun onAudioDevicesAdded(added: Array<out AudioDeviceInfo>) = route()

        override fun onAudioDevicesRemoved(removed: Array<out AudioDeviceInfo>) = route()
    }

    fun enter() {
        if (active) return
        active = true
        manager.mode = AudioManager.MODE_IN_COMMUNICATION
        val request = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_VOICE_COMMUNICATION)
                    .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                    .build(),
            )
            .build()
        manager.requestAudioFocus(request)
        focus = request
        manager.registerAudioDeviceCallback(devices, handler)
        route()
    }

    fun leave() {
        if (!active) return
        active = false
        manager.unregisterAudioDeviceCallback(devices)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            manager.clearCommunicationDevice()
        } else {
            @Suppress("DEPRECATION")
            manager.isSpeakerphoneOn = false
            @Suppress("DEPRECATION")
            manager.stopBluetoothSco()
        }
        manager.mode = AudioManager.MODE_NORMAL
        focus?.let { manager.abandonAudioFocusRequest(it) }
        focus = null
        onOutputs(emptyList(), null)
    }

    fun choose(kind: OutputKind) {
        chosen = kind
        route()
    }

    fun refresh() = route()

    private fun route() {
        if (!active) return
        val available = outputs()
        val target = available.firstOrNull { it.kind == chosen }
            ?: available.firstOrNull { it.kind in HEADSET_KINDS }
            ?: available.firstOrNull { it.kind == OutputKind.SPEAKER }
            ?: available.firstOrNull()
        if (target != null) apply(target)
        onOutputs(available, target)
    }

    private fun outputs(): List<AudioOutput> {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            return manager.availableCommunicationDevices.mapNotNull { device ->
                kindOf(device.type)?.let { AudioOutput(it, device.productName?.toString().orEmpty()).apply { this.device = device } }
            }.distinctBy { it.kind to it.name }
        }
        // Before Android 12: the loudspeaker, the earpiece, and whatever headsets are plugged in or paired.
        val plugged = manager.getDevices(AudioManager.GET_DEVICES_OUTPUTS).mapNotNull { device ->
            kindOf(device.type)?.takeIf { it != OutputKind.SPEAKER && it != OutputKind.EARPIECE }
                ?.let { AudioOutput(it, device.productName?.toString().orEmpty()) }
        }.distinctBy { it.kind }
        return listOf(AudioOutput(OutputKind.SPEAKER, ""), AudioOutput(OutputKind.EARPIECE, "")) + plugged
    }

    private fun apply(output: AudioOutput) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            val device = output.device ?: return
            if (manager.communicationDevice?.id != device.id) manager.setCommunicationDevice(device)
            return
        }
        @Suppress("DEPRECATION")
        when (output.kind) {
            OutputKind.BLUETOOTH -> {
                manager.isSpeakerphoneOn = false
                manager.startBluetoothSco()
                manager.isBluetoothScoOn = true
            }
            else -> {
                if (manager.isBluetoothScoOn) {
                    manager.isBluetoothScoOn = false
                    manager.stopBluetoothSco()
                }
                manager.isSpeakerphoneOn = output.kind == OutputKind.SPEAKER
            }
        }
    }

    private companion object {
        val HEADSET_KINDS = setOf(OutputKind.WIRED, OutputKind.BLUETOOTH, OutputKind.USB)

        fun kindOf(type: Int): OutputKind? = when (type) {
            AudioDeviceInfo.TYPE_BUILTIN_SPEAKER -> OutputKind.SPEAKER
            AudioDeviceInfo.TYPE_BUILTIN_EARPIECE -> OutputKind.EARPIECE
            AudioDeviceInfo.TYPE_WIRED_HEADSET, AudioDeviceInfo.TYPE_WIRED_HEADPHONES -> OutputKind.WIRED
            AudioDeviceInfo.TYPE_BLUETOOTH_SCO, AudioDeviceInfo.TYPE_BLE_HEADSET -> OutputKind.BLUETOOTH
            AudioDeviceInfo.TYPE_USB_HEADSET -> OutputKind.USB
            else -> null
        }
    }
}

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

/**
 * The phone's audio set up for a call while voice runs: communication mode
 * (so the platform's echo cancellation pairs the microphone with what the
 * speaker plays), call audio focus, and routing to a headset when one is
 * connected, else the loudspeaker, as voice chat apps do (a phone call would
 * pick the earpiece).
 */
class CallAudio(context: Context) {
    private val manager = context.getSystemService(AudioManager::class.java)
    private val handler = Handler(Looper.getMainLooper())
    private var focus: AudioFocusRequest? = null
    private var active = false

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
        }
        manager.mode = AudioManager.MODE_NORMAL
        focus?.let { manager.abandonAudioFocusRequest(it) }
        focus = null
    }

    private fun route() {
        if (!active) return
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            val available = manager.availableCommunicationDevices
            val target = available.firstOrNull { it.type in HEADSETS } ?: available.firstOrNull {
                it.type == AudioDeviceInfo.TYPE_BUILTIN_SPEAKER
            }
            if (target != null && manager.communicationDevice?.id != target.id) manager.setCommunicationDevice(target)
        } else {
            val headset = manager.getDevices(AudioManager.GET_DEVICES_OUTPUTS).any { it.type in HEADSETS }
            @Suppress("DEPRECATION")
            manager.isSpeakerphoneOn = !headset
        }
    }

    private companion object {
        val HEADSETS = buildSet {
            add(AudioDeviceInfo.TYPE_WIRED_HEADSET)
            add(AudioDeviceInfo.TYPE_WIRED_HEADPHONES)
            add(AudioDeviceInfo.TYPE_BLUETOOTH_SCO)
            add(AudioDeviceInfo.TYPE_USB_HEADSET)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) add(AudioDeviceInfo.TYPE_BLE_HEADSET)
        }
    }
}

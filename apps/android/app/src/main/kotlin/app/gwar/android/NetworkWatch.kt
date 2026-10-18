package app.gwar.android

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.os.Handler
import android.os.Looper

/**
 * Calls [onChange] when the default network changes (Wi-Fi to mobile data and
 * back): the connection made on the old one is gone, and waiting for its ping
 * to time out would leave the user cut off for up to half a minute.
 */
class NetworkWatch(context: Context, private val onChange: () -> Unit) {
    private val manager = context.getSystemService(ConnectivityManager::class.java)
    private val handler = Handler(Looper.getMainLooper())
    /** The default network; `null` until the first callback after [start]. */
    private var current: Network? = null

    /** The default network went away: the next one is a change, even the same one back. */
    private var lost = false
    private var watching = false

    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            val changed = lost || (current != null && current != network)
            current = network
            lost = false
            if (changed) onChange()
        }

        override fun onLost(network: Network) {
            if (network == current) lost = true
        }
    }

    fun start() {
        if (watching) return
        watching = true
        current = null
        lost = false
        manager.registerDefaultNetworkCallback(callback, handler)
    }

    fun stop() {
        if (!watching) return
        watching = false
        manager.unregisterNetworkCallback(callback)
    }
}

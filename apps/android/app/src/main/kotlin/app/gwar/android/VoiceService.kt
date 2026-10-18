package app.gwar.android

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder

/**
 * Keeps a call going with the screen off or the app in the background: a
 * foreground service of the microphone type, with an ongoing notification
 * that opens the app or leaves the channel. The call itself lives in
 * [GwarClient]; this only keeps the process eligible to record.
 */
class VoiceService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_LEAVE) {
            (application as GwarApplication).client.leaveChannel()
            stopSelf()
            return START_NOT_STICKY
        }
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL, getString(R.string.voice_notification_channel), NotificationManager.IMPORTANCE_LOW),
        )
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP), PendingIntent.FLAG_IMMUTABLE,
        )
        val leave = PendingIntent.getService(
            this, 1, Intent(this, VoiceService::class.java).setAction(ACTION_LEAVE), PendingIntent.FLAG_IMMUTABLE,
        )
        val channel = intent?.getStringExtra(EXTRA_CHANNEL).orEmpty()
        val notification = Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.voice_notification_title, channel))
            .setContentText(getString(R.string.voice_notification_text))
            .setContentIntent(open)
            .setOngoing(true)
            .setCategory(Notification.CATEGORY_CALL)
            .addAction(Notification.Action.Builder(null, getString(R.string.server_leave), leave).build())
            .build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIFICATION, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE)
        } else {
            startForeground(NOTIFICATION, notification)
        }
        return START_NOT_STICKY
    }

    companion object {
        private const val CHANNEL = "voice"
        private const val NOTIFICATION = 1
        private const val ACTION_LEAVE = "app.gwar.android.LEAVE"
        private const val EXTRA_CHANNEL = "channel"

        fun start(context: Context, channel: String) {
            context.startForegroundService(Intent(context, VoiceService::class.java).putExtra(EXTRA_CHANNEL, channel))
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, VoiceService::class.java))
        }
    }
}

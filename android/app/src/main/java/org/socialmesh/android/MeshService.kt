package org.socialmesh.android

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.graphics.drawable.Icon
import android.os.IBinder
import android.util.Log

/**
 * Keeps the node running while the app is in the background, with an
 * ongoing notification (Android requires one) that says how many links are
 * up and has a Stop button.
 */
class MeshService : Service() {
    private var started = false

    private val linkWatcher = Mesh.Listener { line ->
        if (line.startsWith("Connected") || line.startsWith("Disconnected") || line == "Started") showLinks()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        val channel = NotificationChannel(CHANNEL, "Running in the background", NotificationManager.IMPORTANCE_LOW)
        channel.description = "Shown while SocialMesh is finding and syncing with people nearby."
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopSelf()
            return START_NOT_STICKY
        }
        try {
            startForeground(NOTIFICATION_ID, notification("Starting…"), ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
        } catch (e: RuntimeException) {
            // Not allowed right now (started from the background, or a permission was revoked).
            Log.w(TAG, "can't run in the foreground", e)
            stopSelf()
            return START_NOT_STICKY
        }
        if (!started) {
            started = true
            Mesh.addListener(linkWatcher)
            Thread({
                try {
                    Mesh.start(this)
                } catch (e: RuntimeException) {
                    Mesh.report("Could not start: ${e.message}")
                    stopSelf()
                }
            }, "mesh-start").start()
        }
        // Restarted by Android if it kills the process for memory.
        return START_STICKY
    }

    override fun onDestroy() {
        Mesh.removeListener(linkWatcher)
        if (started) Thread(Mesh::stop, "mesh-stop").start()
        super.onDestroy()
    }

    private fun showLinks() {
        val n = Mesh.links()?.lines()?.count { it.isNotBlank() } ?: 0
        val text = when (n) {
            0 -> "Looking for people nearby"
            1 -> "Linked with 1 node"
            else -> "Linked with $n nodes"
        }
        getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification(text))
    }

    private fun notification(text: String): Notification {
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE,
        )
        val stop = PendingIntent.getService(this, 1, stopIntent(this), PendingIntent.FLAG_IMMUTABLE)
        return Notification.Builder(this, CHANNEL)
            .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
            .setContentTitle("SocialMesh is on")
            .setContentText(text)
            .setOngoing(true)
            .setContentIntent(open)
            .addAction(
                Notification.Action.Builder(
                    Icon.createWithResource(this, android.R.drawable.ic_menu_close_clear_cancel), "Stop", stop,
                ).build(),
            )
            .build()
    }

    companion object {
        private const val TAG = "SocialMesh"
        private const val CHANNEL = "running"
        private const val NOTIFICATION_ID = 1
        private const val ACTION_STOP = "org.socialmesh.android.STOP"

        fun startIntent(context: Context) = Intent(context, MeshService::class.java)

        fun stopIntent(context: Context) = Intent(context, MeshService::class.java).setAction(ACTION_STOP)
    }
}

package dev.ryzhub.ryzhub

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build
import com.google.firebase.FirebaseApp
import com.google.firebase.messaging.FirebaseMessaging
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage

/** Firebase Cloud Messaging, when the build carries a Firebase configuration. */
object Push {
    const val CHANNEL = "ryzhub_events"

    /** Told of a new token while the app runs. */
    @Volatile
    var onToken: ((String) -> Unit)? = null

    /** This device's token, or null without Firebase or when it cannot be had. */
    fun token(context: Context, done: (String?) -> Unit) {
        if (FirebaseApp.getApps(context).isEmpty()) {
            done(null)
            return
        }
        FirebaseMessaging.getInstance().token.addOnCompleteListener { task ->
            done(if (task.isSuccessful) task.result else null)
        }
    }

    fun createChannel(context: Context) {
        if (Build.VERSION.SDK_INT < 26) return
        val manager = context.getSystemService(NotificationManager::class.java)
        val channel = NotificationChannel(CHANNEL, "RyzHub", NotificationManager.IMPORTANCE_HIGH)
        channel.description = "Approvals, questions and finished runs"
        manager.createNotificationChannel(channel)
    }

    /** The organization and conversation a notification's intent names. */
    fun extras(intent: Intent?): Map<String, String>? {
        val conversation = intent?.getStringExtra("conversation_id") ?: return null
        return mapOf(
            "conversation_id" to conversation,
            "org_id" to (intent.getStringExtra("org_id") ?: ""),
        )
    }

    /** Show a notification while the app is in front (the system shows the others). */
    fun show(context: Context, title: String?, body: String?, data: Map<String, String>) {
        createChannel(context)
        val open = Intent(context, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
            for ((key, value) in data) putExtra(key, value)
        }
        val conversation = data["conversation_id"] ?: ""
        val pending = PendingIntent.getActivity(
            context,
            conversation.hashCode(),
            open,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val builder = if (Build.VERSION.SDK_INT >= 26) {
            Notification.Builder(context, CHANNEL)
        } else {
            @Suppress("DEPRECATION")
            Notification.Builder(context)
        }
        val notification = builder
            .setSmallIcon(R.mipmap.ic_launcher)
            .setContentTitle(title)
            .setContentText(body)
            .setAutoCancel(true)
            .setContentIntent(pending)
            .build()
        val manager = context.getSystemService(NotificationManager::class.java)
        manager.notify(conversation, 0, notification)
    }
}

class PushService : FirebaseMessagingService() {
    override fun onNewToken(token: String) {
        Push.onToken?.invoke(token)
    }

    override fun onMessageReceived(message: RemoteMessage) {
        val notification = message.notification ?: return
        Push.show(this, notification.title, notification.body, message.data)
    }
}

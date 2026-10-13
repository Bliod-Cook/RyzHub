package dev.ryzhub.ryzhub

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel

class MainActivity : FlutterActivity() {
    private var channel: MethodChannel? = null

    /** The conversation the notification that started the app names, until Dart takes it. */
    private var opened: Map<String, String>? = null

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        Push.createChannel(this)
        opened = Push.extras(intent)
        val channel = MethodChannel(flutterEngine.dartExecutor.binaryMessenger, "ryzhub/push")
        channel.setMethodCallHandler { call, result ->
            when (call.method) {
                "token" -> Push.token(this) { result.success(it) }
                "permit" -> {
                    permit()
                    result.success(null)
                }
                "takeOpened" -> {
                    result.success(opened)
                    opened = null
                }
                else -> result.notImplemented()
            }
        }
        this.channel = channel
        Push.onToken = { token -> runOnUiThread { this.channel?.invokeMethod("token", token) } }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        val extras = Push.extras(intent) ?: return
        channel?.invokeMethod("opened", extras)
    }

    override fun onDestroy() {
        Push.onToken = null
        super.onDestroy()
    }

    /** Android 13 and later ask before an app may notify. */
    private fun permit() {
        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1)
        }
    }
}

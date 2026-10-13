import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

import 'controller.dart';
import 'prefs.dart';

/// Android notifications. After sign-in this device's FCM token goes to the
/// server, which notifies it of approvals, questions and finished runs while
/// it has no live connection; tapping one opens its conversation.
///
/// The Android side (`MainActivity.kt`) answers `null` for the token when the
/// build has no Firebase configuration (`google-services.json`).
class Push {
  static const _channel = MethodChannel('ryzhub/push');

  static bool get supported =>
      !kIsWeb && defaultTargetPlatform == TargetPlatform.android;

  /// Register [hub]'s device; [open] gets the conversation a tapped
  /// notification names.
  static Future<void> start(
    HubController hub,
    void Function(String conversation) open,
  ) async {
    if (!supported || hub.capabilities['push'] != true) return;
    _channel.setMethodCallHandler((call) async {
      switch (call.method) {
        case 'token':
          await _register(hub, call.arguments as String);
        case 'opened':
          final conversation = (call.arguments as Map?)?['conversation_id'];
          if (conversation is String) open(conversation);
      }
    });
    try {
      await _channel.invokeMethod('permit');
      final token = await _channel.invokeMethod<String>('token');
      if (token != null) await _register(hub, token);
      final opened = await _channel.invokeMethod<Map>('takeOpened');
      final conversation = opened?['conversation_id'];
      if (conversation is String) open(conversation);
    } on PlatformException catch (e) {
      debugPrint('notifications unavailable: ${e.message}');
    }
  }

  static Future<void> _register(HubController hub, String token) async {
    try {
      await hub.request(
        '/devices',
        body: {
          'device': hub.device,
          'platform': 'android',
          'token': token,
          'locale': prefs.lang,
        },
      );
    } catch (e) {
      debugPrint('device not registered: $e');
    }
  }

  /// Stop notifying this device, before signing out.
  static Future<void> stop(HubController hub) async {
    if (!supported || hub.capabilities['push'] != true) return;
    try {
      await hub.request('/devices/${hub.device}', method: 'DELETE');
    } catch (_) {
      // Signing out stops them anyway.
    }
  }
}

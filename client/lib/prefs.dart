import 'dart:async';
import 'dart:convert';
import 'dart:ui';

import 'package:flutter/material.dart';

import 'l10n.dart';
import 'storage/prefs_store.dart';

/// Per-client preferences. They belong to this device, not to the account.
class AppPrefs extends ChangeNotifier {
  String language = 'auto', theme = 'system', accent = 'orange';
  bool enterToSend = false, sidebarCollapsed = false;
  String defaultMode = 'chat', defaultHarness = 'codex';

  /// Null follows the server's default model.
  String? defaultChatModel, defaultWorkModel;

  /// Sidebar sections folded on this device: `pinned`, `projects`, `recents`.
  Set<String> collapsedSections = {};
  String _detected = 'en';
  PrefsStore? _store;

  String get lang => language == 'auto' ? _detected : language;
  ThemeMode get themeMode => switch (theme) {
    'light' => ThemeMode.light,
    'dark' => ThemeMode.dark,
    _ => ThemeMode.system,
  };

  Future<void> load() async {
    _detected = detectLanguage(PlatformDispatcher.instance.locales);
    try {
      _store = await PrefsStore.open();
      final raw = await _store!.read();
      if (raw.isEmpty) return;
      final v = jsonDecode(raw) as Map<String, dynamic>;
      language = v['language'] ?? language;
      theme = v['theme'] ?? theme;
      accent = v['accent'] ?? accent;
      enterToSend = v['enter_to_send'] ?? enterToSend;
      sidebarCollapsed = v['sidebar_collapsed'] ?? sidebarCollapsed;
      defaultMode = v['default_mode'] ?? defaultMode;
      defaultChatModel = v['default_chat_model'];
      defaultWorkModel = v['default_work_model'];
      collapsedSections = {
        for (final s in v['collapsed_sections'] as List? ?? []) '$s',
      };
      defaultHarness = v['default_harness'] ?? defaultHarness;
    } catch (_) {
      // Unreadable preferences fall back to defaults; they are not critical.
    }
  }

  void update(void Function(AppPrefs p) change) {
    change(this);
    notifyListeners();
    final store = _store;
    if (store == null) return;
    unawaited(
      store
          .write(
            jsonEncode({
              'language': language,
              'theme': theme,
              'accent': accent,
              'enter_to_send': enterToSend,
              'sidebar_collapsed': sidebarCollapsed,
              'default_mode': defaultMode,
              'default_chat_model': defaultChatModel,
              'default_work_model': defaultWorkModel,
              'collapsed_sections': collapsedSections.toList(),
              'default_harness': defaultHarness,
            }),
          )
          .catchError((_) {}),
    );
  }

  void localesChanged(List<Locale> locales) {
    final detected = detectLanguage(locales);
    if (detected == _detected) return;
    _detected = detected;
    notifyListeners();
  }
}

final prefs = AppPrefs();

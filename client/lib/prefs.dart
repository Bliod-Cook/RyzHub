import 'dart:async';
import 'dart:convert';
import 'dart:ui';

import 'package:flutter/material.dart';

import 'l10n.dart';
import 'storage/prefs_store.dart';

/// Per-client preferences. They belong to this device, not to the account.
class AppPrefs extends ChangeNotifier {
  String language = 'auto', theme = 'system', accent = 'orange';
  bool enterToSend = true, sidebarCollapsed = false;

  /// The file library shows rows instead of a grid of cards.
  bool libraryList = false;
  String defaultMode = 'chat', defaultHarness = 'codex';

  /// Null follows the server's default model.
  String? defaultChatModel, defaultWorkModel;

  /// Sidebar sections folded on this device: `pinned`, `projects`, `recents`.
  Set<String> collapsedSections = {};

  /// The server this device last signed in to.
  String server = '';

  /// The conversation last open per `server org`, reopened on a native start.
  /// The web build keeps it in the address instead.
  Map<String, String> lastConversations = {};
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
    final String raw;
    try {
      _store = await PrefsStore.open();
      raw = await _store!.read();
    } catch (_) {
      // A store the platform cannot open keeps the defaults; preferences are
      // not critical.
      return;
    }
    read(raw);
  }

  /// Takes the preferences stored as [raw]; each one missing or unreadable
  /// keeps its default.
  @visibleForTesting
  void read(String raw) {
    final Object? decoded;
    try {
      decoded = jsonDecode(raw);
    } on FormatException {
      return;
    }
    if (decoded is! Map) return;
    final stored = decoded;
    T? at<T>(String key) => stored[key] is T ? stored[key] as T : null;
    language = at<String>('language') ?? language;
    theme = at<String>('theme') ?? theme;
    accent = at<String>('accent') ?? accent;
    enterToSend = at<bool>('enter_sends') ?? enterToSend;
    sidebarCollapsed = at<bool>('sidebar_collapsed') ?? sidebarCollapsed;
    libraryList = at<bool>('library_list') ?? libraryList;
    defaultMode = at<String>('default_mode') ?? defaultMode;
    defaultChatModel = at<String>('default_chat_model');
    defaultWorkModel = at<String>('default_work_model');
    collapsedSections = {
      for (final s in at<List>('collapsed_sections') ?? []) '$s',
    };
    defaultHarness = at<String>('default_harness') ?? defaultHarness;
    server = at<String>('server') ?? server;
    lastConversations = {
      for (final e in (at<Map>('last_conversations') ?? {}).entries)
        '${e.key}': '${e.value}',
    };
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
              'enter_sends': enterToSend,
              'sidebar_collapsed': sidebarCollapsed,
              'library_list': libraryList,
              'default_mode': defaultMode,
              'default_chat_model': defaultChatModel,
              'default_work_model': defaultWorkModel,
              'collapsed_sections': collapsedSections.toList(),
              'default_harness': defaultHarness,
              'server': server,
              'last_conversations': lastConversations,
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

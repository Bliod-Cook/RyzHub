import 'dart:convert';

import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/l10n.dart';
import 'package:ryzhub/prefs.dart';

void main() {
  test('language follows the first Chinese or English device locale', () {
    expect(detectLanguage(const [Locale('zh', 'TW')]), 'zh');
    expect(detectLanguage(const [Locale('fr', 'FR')]), 'en');
    expect(detectLanguage(const []), 'en');
    expect(detectLanguage(const [Locale('ja'), Locale('zh', 'CN')]), 'zh');
    expect(detectLanguage(const [Locale('en', 'US'), Locale('zh')]), 'en');
  });

  test('an unreadable preference keeps its default and spares the rest', () {
    final read = AppPrefs()
      ..read(
        jsonEncode({
          'theme': 3,
          'accent': 'blue',
          'collapsed_sections': 'pinned',
          'last_conversations': {'s o': 'c'},
        }),
      );
    expect(read.theme, 'system');
    expect(read.accent, 'blue');
    expect(read.collapsedSections, isEmpty);
    expect(read.lastConversations, {'s o': 'c'});
    expect((AppPrefs()..read('{')).accent, 'orange');
  });
}

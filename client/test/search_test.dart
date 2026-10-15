import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/prefs.dart';
import 'package:ryzhub/ui/dialogs.dart';

import 'kernel.dart';

/// A hub whose history and search requests wait until the test answers them.
class _Hub extends HubController {
  final pending = <String, Completer<dynamic>>{};
  @override
  Future<void> select(String id) async {
    selected = id;
  }

  @override
  Future<dynamic> request(
    String path, {
    Map<String, dynamic>? body,
    String? method,
    Duration timeout = const Duration(seconds: 15),
  }) async {
    if (path.contains('/items?before_seq=')) {
      return (pending['history'] ??= Completer<dynamic>()).future;
    }
    final query = Uri.parse(path).queryParameters['q']!;
    return (pending[query] ??= Completer<dynamic>()).future;
  }
}

void main() {
  setUp(() => prefs.language = 'zh');
  test('search focus uses an independent window and leaves the latest cache intact', () async {
    final hub = _Hub();
    addTearDown(hub.dispose);
    final selected = hub.selectSearch('conversation', 'hit', 42);
    await Future<void>.delayed(Duration.zero);
    hub.pending['history']!.complete({
      'items': [
        {'id': 'hit', 'first_seq': 42},
      ],
      'has_more': true,
    });
    await selected;
    expect(hub.items.single['id'], 'hit');
    expect(hub.hasOlder, isTrue);
    hub.clearSearchFocus();
    expect(hub.items, isEmpty);
  });
  testWidgets(
    'remote search discards an older response and keeps hit navigation',
    (tester) async {
      final hub = _Hub()..capabilities = {'search': true};
      addTearDown(hub.dispose);
      await tester.pumpWidget(
        MaterialApp(
          theme: hubTheme,
          home: Scaffold(body: SearchDialog(hub: hub)),
        ),
      );
      await tester.enterText(find.byType(TextField), 'old');
      await tester.pump(const Duration(milliseconds: 350));
      await tester.enterText(find.byType(TextField), 'new');
      await tester.pump(const Duration(milliseconds: 350));
      hub.pending['new']!.complete({
        'matches': [
          {
            'text': '新的命中',
            'conversation_id': 'conversation',
            'item_id': 'item',
            'seq': 42,
          },
        ],
      });
      await tester.pump();
      hub.pending['old']!.complete({
        'matches': [
          {'text': '过期命中', 'conversation_id': 'old', 'seq': 1},
        ],
      });
      await tester.pump();
      expect(find.text('新的命中'), findsOneWidget);
      expect(find.text('过期命中'), findsNothing);
      await tester.pumpWidget(const SizedBox());
    },
  );
}

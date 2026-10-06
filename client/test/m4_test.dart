import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/prefs.dart';
import 'package:ryzhub/ui/dialogs.dart';
import 'package:ryzhub/ui/memories.dart';
import 'package:ryzhub/ui/messages.dart';

class _Hub extends HubController {
  final pending = <String, Completer<dynamic>>{};
  final requests = <Map<String, dynamic>>[];
  @override
  Future<void> select(String id) async {
    selected = id;
  }

  @override
  Future<dynamic> request(
    String path, {
    Map<String, dynamic>? body,
    String? method,
  }) async {
    requests.add({'path': path, 'body': body, 'method': method});
    if (path.contains('/items?before_seq=')) {
      return (pending['history'] ??= Completer<dynamic>()).future;
    }
    if (path.contains('/search?')) {
      final query = Uri.parse(path).queryParameters['q']!;
      return (pending[query] ??= Completer<dynamic>()).future;
    }
    if (body == null) {
      return {
        'items': [],
        'preferences': {'enabled': true, 'reference_history': true},
        'policy_enabled': true,
      };
    }
    return {};
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
  test('memory context stays out of the conversation', () {
    final rows = timeline([
      {
        'id': 'context',
        'kind': 'memory_context',
        'state': 'completed',
        'text': '[{"content":"old secret',
        'run_id': 'run',
      },
      {
        'id': 'reply',
        'kind': 'assistant',
        'state': 'completed',
        'text': 'hi',
        'run_id': 'run',
      },
    ]);
    expect(rows.map((r) => r['id']), ['reply']);
  });
  testWidgets(
    'remote search discards an older response and keeps hit navigation',
    (tester) async {
      final hub = _Hub()..capabilities = {'search': true};
      addTearDown(hub.dispose);
      await tester.pumpWidget(
        MaterialApp(
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
  testWidgets('memory preferences save both independent switches', (
    tester,
  ) async {
    final hub = _Hub();
    addTearDown(hub.dispose);
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: SingleChildScrollView(child: MemorySettings(hub: hub)),
        ),
      ),
    );
    await tester.pumpAndSettle();
    await tester.tap(find.text('引用历史对话'));
    await tester.pumpAndSettle();
    final update = hub.requests.firstWhere((r) => r['method'] == 'PATCH');
    expect(update['body'], {'enabled': true, 'reference_history': false});
  });
  testWidgets('memory write hint exposes undo without dumping tool payloads', (
    tester,
  ) async {
    var undone = false;
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: MessageCard(
            item: {
              'kind': 'memory_notice',
              'state': 'completed',
              'text': jsonEncode({
                'action': 'add',
                'event_id': 'event',
                'content': 'private details',
              }),
            },
            onMemoryUndo: () => undone = true,
          ),
        ),
      ),
    );
    expect(find.text('记忆已更新'), findsOneWidget);
    expect(find.text('private details'), findsNothing);
    await tester.tap(find.text('撤销'));
    expect(undone, isTrue);
  });
}

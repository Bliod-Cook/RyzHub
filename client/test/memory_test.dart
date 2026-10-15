import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/prefs.dart';
import 'package:ryzhub/ui/memories.dart';

import 'kernel.dart';

/// A hub that records its requests and has no memories yet.
class _Hub extends HubController {
  final requests = <Map<String, dynamic>>[];

  @override
  Future<dynamic> request(
    String path, {
    Map<String, dynamic>? body,
    String? method,
    Duration timeout = const Duration(seconds: 15),
  }) async {
    requests.add({'path': path, 'body': body, 'method': method});
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
  testWidgets('memory preferences save both independent switches', (
    tester,
  ) async {
    final hub = _Hub();
    addTearDown(hub.dispose);
    await tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
        home: Scaffold(
          body: SingleChildScrollView(child: MemorySettings(hub: hub)),
        ),
      ),
    );
    await tester.pumpAndSettle();
    await tester.tap(find.byType(Switch).last);
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
        theme: hubTheme,
        home: Scaffold(
          body: messageCard(
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

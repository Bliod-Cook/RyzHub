import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/main.dart';

/// A hub whose requests wait until the test answers them.
class _Hub extends HubController {
  final answers = <String, Completer<dynamic>>{};
  _Hub() {
    org = 'org';
    capabilities = {'work': 'real'};
    conversations.addAll([
      {'id': 'a', 'mode': 'work'},
      {'id': 'b', 'mode': 'work'},
    ]);
  }

  @override
  Future<dynamic> request(
    String path, {
    Map<String, dynamic>? body,
    String? method,
  }) => (answers[path] = Completer()).future;
}

Future<void> _composer(
  WidgetTester tester,
  Future<void> Function(String) onSend,
) => tester.pumpWidget(
  MaterialApp(
    home: Scaffold(
      body: Composer(onSend: onSend, onCancel: () {}, running: false),
    ),
  ),
);

void main() {
  setUpAll(() => prefs.language = 'en');

  testWidgets('what is typed while a message goes out stays', (tester) async {
    final sending = Completer<void>();
    final sent = <String>[];
    await _composer(tester, (text) {
      sent.add(text);
      return sending.future;
    });
    await tester.enterText(find.byType(TextField), 'first message');
    await tester.tap(find.byTooltip('Send'));
    await tester.pump();
    expect(sent, ['first message']);
    // The field is free at once for the next draft.
    expect(find.text('first message'), findsNothing);
    await tester.enterText(find.byType(TextField), 'next draft');
    sending.complete();
    await tester.pumpAndSettle();
    expect(find.text('next draft'), findsOneWidget);
  });

  testWidgets(
    'a message that fails to go out comes back before the new draft',
    (tester) async {
      final sending = Completer<void>();
      await _composer(tester, (_) => sending.future);
      await tester.enterText(find.byType(TextField), 'first message');
      await tester.tap(find.byTooltip('Send'));
      await tester.pump();
      await tester.enterText(find.byType(TextField), 'next draft');
      sending.completeError(Exception('offline'));
      await tester.pumpAndSettle();
      expect(find.text('first message\nnext draft'), findsOneWidget);
    },
  );

  test('a refused message comes back to edit; accepted ones are forgotten', () {
    final hub = HubController();
    const files = [
      {'id': 'f1', 'name': 'notes.csv', 'size': 5, 'kind': 'text'},
    ];
    hub.debugQueued(
      'c1',
      const SentInput(channel: 'conv:a', text: 'read this', files: files),
    );
    hub.debugQueued(
      'c2',
      const SentInput(channel: 'conv:a', text: 'and this', files: []),
    );
    hub.debugQueued(
      'c3',
      const SentInput(channel: 'conv:a', text: 'busy', files: []),
    );
    hub.debugResults(
      '[{"cid":"c2","channel":"conv:a","accepted":true,"code":"","retryable":false},'
      '{"cid":"c3","channel":"conv:a","accepted":false,"code":"RATE_LIMITED","retryable":true},'
      '{"cid":"c1","channel":"conv:a","accepted":false,"code":"ATTACHMENT_UNAVAILABLE","retryable":false}]',
    );
    final failed = hub.takeFailedSend()!;
    expect(failed.text, 'read this');
    expect(failed.files.single['id'], 'f1');
    expect(failed.code, 'ATTACHMENT_UNAVAILABLE');
    expect(hub.takeFailedSend(), isNull);
    // A retryable refusal stays on its way; its later refusal still finds it.
    hub.debugResults(
      '[{"cid":"c3","channel":"conv:a","accepted":false,"code":"CONFLICT","retryable":false}]',
    );
    expect(hub.takeFailedSend()!.text, 'busy');
    // A command with no message behind it reports as before.
    hub.debugResults(
      '[{"cid":"zz","channel":"conv:a","accepted":false,"code":"FORBIDDEN","retryable":false}]',
    );
    expect(hub.error, 'FORBIDDEN');
  });

  test('a refused setting drops only that conversation\'s changes', () {
    final hub = HubController()..selected = 'a';
    hub.debugPendingConfig('conv:a', {'reasoning': 'high'});
    hub.debugPendingConfig('conv:b', {'reasoning': 'low'});
    hub.debugResults(
      '[{"cid":"x","channel":"conv:b","accepted":false,"code":"INVALID_REASONING","retryable":false}]',
    );
    expect(hub.settings['reasoning'], 'high');
    hub.selected = 'b';
    expect(hub.settings['reasoning'], isNull);
  });

  test(
    'a workspace answer for a conversation left behind is dropped',
    () async {
      final hub = _Hub()..selected = 'a';
      final first = hub.refreshWorkspace();
      hub.selected = 'b';
      final second = hub.refreshWorkspace();
      hub.answers['/orgs/org/conversations/b/workspace']!.complete({'id': 'b'});
      await second;
      hub.answers['/orgs/org/conversations/a/workspace']!.complete({'id': 'a'});
      await first;
      expect(hub.workspace['id'], 'b');
    },
  );
}

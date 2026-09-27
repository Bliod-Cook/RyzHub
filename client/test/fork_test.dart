import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/main.dart';

void main() {
  setUpAll(() => prefs.language = 'zh');
  testWidgets('Codex exposes only server-confirmed fork boundaries', (
    tester,
  ) async {
    final hub = HubController();
    addTearDown(hub.dispose);
    hub.commandCatalog = {
      'settings': {'harness': 'codex'},
      'capabilities': {'fork': 'native'},
      'fork_points': ['final'],
    };
    final selected = <String>[];
    final replies = [
      for (final id in ['early', 'final'])
        {
          'id': id,
          'kind': 'assistant',
          'state': 'completed',
          'text': id,
          'run_id': 'one-native-turn',
        },
    ];
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: Column(
            children: [
              for (final reply in replies)
                MessageCard(
                  item: reply,
                  onFork: hub.canFork(reply)
                      ? () => selected.add(reply['id']!)
                      : null,
                ),
            ],
          ),
        ),
      ),
    );
    expect(find.byIcon(Icons.fork_right), findsOneWidget);
    await tester.tap(find.byIcon(Icons.fork_right));
    expect(selected, ['final']);
    hub.commandCatalog.remove('fork_points');
    expect(hub.canFork(replies.last), isFalse);
  });
}

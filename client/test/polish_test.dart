import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/ui/common.dart';
import 'package:ryzhub/ui/composer.dart';
import 'package:ryzhub/ui/workspace_status.dart';

Widget _page(Widget child) => MaterialApp(
  home: Scaffold(body: Center(child: child)),
);

void main() {
  testWidgets('the approval mode menu explains each mode and picks one', (
    tester,
  ) async {
    final picked = <String>[];
    await tester.pumpWidget(
      _page(
        ApprovalMenu(
          value: 'ask',
          modes: const ['ask', 'allow_edits'],
          onSelected: picked.add,
        ),
      ),
    );
    expect(find.text('Ask every time'), findsOneWidget);
    await tester.tap(find.text('Ask every time'));
    await tester.pumpAndSettle();
    expect(
      find.text('Edits files without asking; still asks before commands'),
      findsOneWidget,
    );
    // The policy took full auto away.
    expect(find.text('Full auto'), findsNothing);
    await tester.tap(find.text('Auto-allow edits'));
    await tester.pumpAndSettle();
    expect(picked, ['allow_edits']);
  });

  testWidgets('an emulated control says what stands in for it', (tester) async {
    await tester.pumpWidget(_page(const EmulatedNote('steer')));
    expect(find.byIcon(Icons.info_outline), findsOneWidget);
    expect(
      find.byTooltip(
        "This agent can't take instructions mid-turn: they're sent as the next turn.",
      ),
      findsOneWidget,
    );
    await tester.pumpWidget(_page(const EmulatedNote('todo')));
    expect(find.byIcon(Icons.info_outline), findsNothing);
  });

  test('checkpoints show how long ago they were saved', () {
    final now = DateTime.utc(2026, 10, 12, 12);
    String saved(Duration ago) =>
        checkpointSaved(now.subtract(ago).toIso8601String(), now: now);
    expect(saved(const Duration(seconds: 20)), 'Checkpoint saved · just now');
    expect(saved(const Duration(minutes: 7)), 'Checkpoint saved · 7 min ago');
    expect(saved(const Duration(hours: 3)), 'Checkpoint saved · 3 h ago');
    expect(
      saved(const Duration(days: 2)),
      startsWith('Checkpoint saved · Oct 10'),
    );
    expect(checkpointSaved(null), 'Checkpoint saved');
    expect(storageNote('hot'), isNull);
    expect(storageNote('deleted'), contains('retention'));
  });
}

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/ui/common.dart';
import 'package:ryzhub/ui/composer.dart';
import 'package:ryzhub/ui/messages.dart';
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

  testWidgets('a user bubble fits its text, up to most of the row', (
    tester,
  ) async {
    final box = find.byWidgetPredicate(
      (w) =>
          w is Container &&
          w.decoration is BoxDecoration &&
          (w.decoration as BoxDecoration).borderRadius ==
              BorderRadius.circular(22),
    );
    Future<double> bubble(String text) async {
      await tester.pumpWidget(
        MaterialApp(
          home: Scaffold(
            body: SizedBox(
              width: 800,
              child: MessageCard(
                item: {
                  'id': 'u',
                  'kind': 'user',
                  'state': 'completed',
                  'text': text,
                },
              ),
            ),
          ),
        ),
      );
      await tester.pump();
      return tester.getSize(box).width;
    }

    expect(await bubble('整理成一份文件给我'), lessThan(300));
    expect(await bubble('很长的一段话' * 40), 800 * .78);

    // As much space above the text as below it.
    await bubble('做这题\n\n第二段');
    final outer = tester.getRect(box);
    final lines = find.descendant(of: box, matching: find.byType(RichText));
    expect(lines, findsNWidgets(2));
    expect(
      tester.getRect(lines.first).top - outer.top,
      outer.bottom - tester.getRect(lines.last).bottom,
    );
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

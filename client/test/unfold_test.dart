import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/prefs.dart';
import 'package:ryzhub/ui/unfold.dart';

import 'kernel.dart';

void main() {
  tearDown(() => prefs.language = 'zh');

  final short = 'A long line of thought. ' * 60;

  /// A reversed timeline, like a conversation's, with a folded group between
  /// [below] rows and, unless shrink-wrapped like a short chat, many above.
  Future<void> timeline(
    WidgetTester tester, {
    bool shrinkWrap = false,
    String? thought,
    int below = 5,
    Widget? fold,
  }) async {
    prefs.language = 'en';
    final anchor = UnfoldAnchor();
    await tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
        home: Scaffold(
          body: Align(
            alignment: Alignment.topCenter,
            child: UnfoldScope(
              anchor: anchor,
              child: CustomScrollView(
                reverse: true,
                shrinkWrap: shrinkWrap,
                physics: anchor.physics,
                slivers: [
                  TimelineRows(
                    delegate: SliverChildBuilderDelegate(
                      (context, i) => i == below
                          ? fold ??
                                stepsGroup(
                                  steps: [
                                    {
                                      'id': 'r',
                                      'kind': 'reasoning',
                                      'state': 'completed',
                                      'text': thought ?? short,
                                    },
                                  ],
                                )
                          : SizedBox(height: 60, child: Text('row $i')),
                      childCount: below + (shrinkWrap ? 2 : 25),
                    ),
                  ),
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }

  double top(WidgetTester tester, String text) =>
      tester.getTopLeft(find.text(text)).dy;

  testWidgets('a group opens downward and folds back without moving what is '
      'above it', (tester) async {
    await timeline(tester);
    final header = top(tester, 'Reasoned');
    final above = top(tester, 'row 6');
    final below = top(tester, 'row 4');
    await tester.tap(find.text('Reasoned'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    // Halfway open: what is above has not moved, what is below has.
    expect(top(tester, 'Reasoned'), moreOrLessEquals(header));
    expect(top(tester, 'row 6'), moreOrLessEquals(above));
    final pushed = find.text('row 4');
    expect(pushed.evaluate().isEmpty || top(tester, 'row 4') > below, isTrue);
    await tester.pumpAndSettle();
    expect(top(tester, 'Reasoned'), moreOrLessEquals(header));
    expect(find.textContaining('A long line of thought.'), findsOneWidget);

    await tester.tap(find.text('Reasoned'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    // Folding, the thought is still there while it slides up.
    expect(find.textContaining('A long line of thought.'), findsOneWidget);
    expect(top(tester, 'Reasoned'), moreOrLessEquals(header));
    await tester.pumpAndSettle();
    expect(find.textContaining('A long line of thought.'), findsNothing);
    expect(top(tester, 'Reasoned'), moreOrLessEquals(header));
    expect(top(tester, 'row 4'), moreOrLessEquals(below));
  });

  testWidgets('a long code block unfolds and folds without moving what is '
      'above it', (tester) async {
    final code = [for (var i = 1; i <= 30; i++) 'x = $i'].join('\n');
    await timeline(
      tester,
      below: 2,
      fold: messageCard(
        item: {
          'id': 'a',
          'kind': 'assistant',
          'state': 'completed',
          'text': '```python\n$code\n```',
        },
      ),
    );
    final header = top(tester, 'python');
    final above = top(tester, 'row 3');
    await tester.tap(find.text('Show all 30 lines'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    expect(top(tester, 'python'), moreOrLessEquals(header));
    await tester.pumpAndSettle();
    expect(top(tester, 'python'), moreOrLessEquals(header));
    expect(top(tester, 'row 3'), moreOrLessEquals(above));
    expect(find.text('Show all 30 lines'), findsNothing);

    await tester.tap(find.byTooltip('Collapse code'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    expect(top(tester, 'python'), moreOrLessEquals(header));
    await tester.pumpAndSettle();
    expect(top(tester, 'python'), moreOrLessEquals(header));
    expect(top(tester, 'row 3'), moreOrLessEquals(above));
    expect(find.text('Show all 30 lines'), findsOneWidget);
  });

  testWidgets('in a short timeline the group still opens downward', (
    tester,
  ) async {
    await timeline(tester, shrinkWrap: true);
    final header = top(tester, 'Reasoned');
    await tester.tap(find.text('Reasoned'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    expect(top(tester, 'Reasoned'), moreOrLessEquals(header));
    await tester.pumpAndSettle();
    expect(top(tester, 'Reasoned'), moreOrLessEquals(header));
    await tester.tap(find.text('Reasoned'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    expect(top(tester, 'Reasoned'), moreOrLessEquals(header));
    await tester.pumpAndSettle();
    expect(top(tester, 'Reasoned'), moreOrLessEquals(header));
  });

  testWidgets('near the top, folding a tall group back leaves the page still', (
    tester,
  ) async {
    // Like the first exchange of a chat: a long answer below the group, and
    // only the question above it to scroll to.
    await timeline(
      tester,
      shrinkWrap: true,
      thought: 'A long line of thought. ' * 600,
      below: 20,
    );
    // Scrolled up to the question.
    await tester.drag(find.byType(Scrollable), const Offset(0, 2000));
    await tester.pumpAndSettle();
    final header = top(tester, 'Reasoned');
    final above = top(tester, 'row 21');
    final below = top(tester, 'row 19');
    await tester.tap(find.text('Reasoned'));
    await tester.pumpAndSettle();
    expect(top(tester, 'Reasoned'), moreOrLessEquals(header));
    await tester.tap(find.text('Reasoned'));
    await tester.pump();
    for (var i = 0; i < 5; i++) {
      await tester.pump(const Duration(milliseconds: 50));
      expect(top(tester, 'Reasoned'), moreOrLessEquals(header));
      expect(top(tester, 'row 21'), moreOrLessEquals(above));
    }
    await tester.pumpAndSettle();
    expect(top(tester, 'Reasoned'), moreOrLessEquals(header));
    expect(top(tester, 'row 19'), moreOrLessEquals(below));
  });

  /// Lays out a conversation's timeline as the app does: [rows] newest first,
  /// by name and height, and past the oldest a loader for older rows.
  Future<void> Function(List<(String, double)> rows) conversation(
    WidgetTester tester,
    ScrollController scroll,
  ) {
    final anchor = UnfoldAnchor();
    return (rows) {
      final index = {
        for (final (i, (name, _)) in rows.indexed) name: i,
        'older': rows.length,
      };
      return tester.pumpWidget(
        MaterialApp(
          theme: hubTheme,
          home: Scaffold(
            body: Align(
              alignment: Alignment.topCenter,
              child: UnfoldScope(
                anchor: anchor,
                child: CustomScrollView(
                  controller: scroll,
                  reverse: true,
                  shrinkWrap: true,
                  physics: anchor.physics,
                  slivers: [
                    SliverPadding(
                      padding: const EdgeInsets.fromLTRB(24, 16, 24, 8),
                      sliver: TimelineRows(
                        loader: true,
                        delegate: SliverChildBuilderDelegate(
                          (context, i) => i == rows.length
                              ? const SizedBox(
                                  key: ValueKey('older'),
                                  height: 40,
                                )
                              : SizedBox(
                                  key: ValueKey(rows[i].$1),
                                  height: rows[i].$2,
                                  child: Text(rows[i].$1),
                                ),
                          childCount: rows.length + 1,
                          findChildIndexCallback: (key) =>
                              key is ValueKey ? index[key.value] : null,
                        ),
                      ),
                    ),
                  ],
                ),
              ),
            ),
          ),
        ),
      );
    };
  }

  final history = [for (var i = 1; i <= 20; i++) ('row $i', 120.0)];

  testWidgets('scrolled back, a reply growing below leaves the page still', (
    tester,
  ) async {
    final scroll = ScrollController();
    final show = conversation(tester, scroll);
    await show([('reply', 200), ...history]);
    scroll.jumpTo(180);
    await tester.pump();
    final above = top(tester, 'row 1'), reply = top(tester, 'reply');
    await show([('reply', 225), ...history]);
    expect(top(tester, 'row 1'), moreOrLessEquals(above));
    expect(top(tester, 'reply'), moreOrLessEquals(reply));
    expect(scroll.offset, moreOrLessEquals(205));

    // Back at the newest, the reply pushes the older rows up again.
    scroll.jumpTo(0);
    await tester.pump();
    final bottom = tester.getBottomLeft(find.byKey(const ValueKey('reply')));
    final pushed = top(tester, 'row 1');
    await show([('reply', 250), ...history]);
    expect(scroll.offset, 0);
    expect(
      tester.getBottomLeft(find.byKey(const ValueKey('reply'))).dy,
      moreOrLessEquals(bottom.dy),
    );
    expect(top(tester, 'row 1'), moreOrLessEquals(pushed - 25));
  });

  testWidgets('scrolled back into a long reply, what shows of it stays as it '
      'grows', (tester) async {
    final scroll = ScrollController();
    final show = conversation(tester, scroll);
    await show([('reply', 2000), ...history]);
    scroll.jumpTo(180);
    await tester.pump();
    final reply = top(tester, 'reply');
    await show([('reply', 2025), ...history]);
    expect(top(tester, 'reply'), moreOrLessEquals(reply));
    expect(scroll.offset, moreOrLessEquals(205));
  });

  testWidgets('scrolled back, rows arriving below leave the page still', (
    tester,
  ) async {
    final scroll = ScrollController();
    final show = conversation(tester, scroll);
    await show([('reply', 200), ...history]);
    scroll.jumpTo(180);
    await tester.pump();
    final above = top(tester, 'row 1');
    await show([('steps', 80), ('reply', 200), ...history]);
    expect(top(tester, 'row 1'), moreOrLessEquals(above));
    expect(scroll.offset, moreOrLessEquals(260));
  });

  testWidgets('older rows loading in at the top leave the page still', (
    tester,
  ) async {
    final scroll = ScrollController();
    final show = conversation(tester, scroll);
    await show([('reply', 200), ...history]);
    scroll.jumpTo(scroll.position.maxScrollExtent);
    await tester.pump();
    final oldest = top(tester, 'row 20');
    await show([
      ('reply', 200),
      ...history,
      for (var i = 21; i <= 25; i++) ('row $i', 120.0),
    ]);
    expect(top(tester, 'row 20'), moreOrLessEquals(oldest));
  });

  testWidgets('what shrinks below by more than the page is scrolled back '
      'lands it at the newest', (tester) async {
    final scroll = ScrollController();
    final show = conversation(tester, scroll);
    await show([('reply', 400), ...history]);
    scroll.jumpTo(180);
    await tester.pump();
    final above = top(tester, 'row 1');
    await show([('reply', 100), ...history]);
    expect(scroll.offset, 0);
    expect(top(tester, 'row 1'), moreOrLessEquals(above + 120));
    await tester.pumpAndSettle();
    expect(scroll.offset, 0);
  });
}

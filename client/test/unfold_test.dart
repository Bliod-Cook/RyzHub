import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/prefs.dart';
import 'package:ryzhub/theme.dart';
import 'package:ryzhub/ui/messages.dart';
import 'package:ryzhub/ui/unfold.dart';

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
        home: Scaffold(
          body: Align(
            alignment: Alignment.topCenter,
            child: UnfoldScope(
              anchor: anchor,
              child: ListView.builder(
                reverse: true,
                shrinkWrap: shrinkWrap,
                physics: anchor.physics,
                itemCount: below + (shrinkWrap ? 2 : 25),
                itemBuilder: (context, i) => i == below
                    ? fold ??
                          StepsGroup(
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
      fold: MessageCard(
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
    await tester.drag(find.byType(ListView), const Offset(0, 2000));
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

  testWidgets("the user's bubble takes the accent", (tester) async {
    Future<Color?> bubble(String accent) async {
      await tester.pumpWidget(
        MaterialApp(
          theme: ThemeData(extensions: [HubColors.light(accentColor(accent))]),
          home: const Scaffold(
            body: MessageCard(
              item: {
                'id': 'u',
                'kind': 'user',
                'state': 'completed',
                'text': 'hello',
              },
            ),
          ),
        ),
      );
      // The theme eases from the last accent to this one.
      await tester.pumpAndSettle();
      final box = tester.widget<Container>(
        find
            .ancestor(of: find.text('hello'), matching: find.byType(Container))
            .first,
      );
      return (box.decoration as BoxDecoration?)?.color;
    }

    final blue = await bubble('blue'), pink = await bubble('pink');
    expect(blue, HubColors.light(accentColor('blue')).userBubble);
    expect(pink, HubColors.light(accentColor('pink')).userBubble);
    expect(blue, isNot(pink));
  });
}

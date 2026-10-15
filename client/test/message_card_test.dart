import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/prefs.dart';
import 'package:ryzhub/theme.dart';

import 'kernel.dart';

void main() {
  setUpAll(() => prefs.language = 'en');

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
          theme: hubTheme,
          home: Scaffold(
            body: SizedBox(
              width: 800,
              child: messageCard(
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

  testWidgets("the user's bubble takes the accent", (tester) async {
    Future<Color?> bubble(String accent) async {
      await tester.pumpWidget(
        MaterialApp(
          theme: ThemeData(
            extensions: [HubColors.light.copyWith(accent: accentColor(accent))],
          ),
          home: Scaffold(
            body: messageCard(
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
    expect(
      blue,
      HubColors.light.copyWith(accent: accentColor('blue')).userBubble,
    );
    expect(
      pink,
      HubColors.light.copyWith(accent: accentColor('pink')).userBubble,
    );
    expect(blue, isNot(pink));
  });

  testWidgets('a reply renders task lists and quotes', (tester) async {
    await tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
        home: Scaffold(
          body: SingleChildScrollView(
            child: messageCard(
              item: {
                'id': 'r',
                'kind': 'assistant',
                'state': 'completed',
                'text': '- [x] done\n- todo\n\n> quoted **bold**',
              },
            ),
          ),
        ),
      ),
    );
    expect(find.byIcon(Icons.check_box), findsOneWidget);
    expect(find.textContaining('quoted', findRichText: true), findsOneWidget);
    expect(shownText(tester), isNot(contains('>')));
    expect(tester.takeException(), isNull);
  });

  testWidgets('citation links fold into chips that open their pages', (
    tester,
  ) async {
    const news = 'https://www.news.cn/a?utm_source=openai';
    const ap = 'https://apnews.com/b';
    const wiki = 'https://en.wikipedia.org/wiki/Asian_Games_(2026)';
    final opened = <String>[];
    await tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
        home: Scaffold(
          body: messageCard(
            item: {
              'id': 'answer',
              'kind': 'assistant',
              'state': 'completed',
              'text':
                  '体育消息。([news.cn]($news))\n\n'
                  '巴西消息。([apnews.com]($ap)) ([wikipedia.org]($wiki))\n\n'
                  '再次提到 ([apnews.com]($ap), [news.cn]($news))，'
                  '另见 [普通链接](https://plain.example)。',
              'detail': {
                'kind': {
                  'Answer': {
                    'citations': [
                      {'url': news, 'title': ''},
                      {'url': ap, 'title': 'Lula seeks shake-up'},
                      {'url': wiki, 'title': 'Asian Games'},
                      {
                        'url': 'https://unlinked.example/p',
                        'title': 'Unlinked',
                      },
                    ],
                  },
                },
              },
            },
            onSource: opened.add,
          ),
        ),
      ),
    );
    final shown = shownText(tester);
    expect(shown, contains('体育消息。'));
    expect(shown, contains('普通链接'));
    expect(shown, isNot(contains('](')));
    expect(find.text('news.cn'), findsOneWidget);
    expect(find.text('apnews.com +1'), findsNWidgets(2));
    // Only the page the text does not link is listed below it.
    expect(find.text('Sources'), findsOneWidget);
    expect(find.textContaining('Unlinked'), findsOneWidget);
    expect(find.textContaining('Lula seeks shake-up'), findsNothing);
    await tester.tap(find.text('news.cn'));
    expect(opened, [news]);
    await tester.tap(find.text('apnews.com +1').first);
    await tester.pumpAndSettle();
    expect(find.text('Lula seeks shake-up'), findsOneWidget);
    await tester.tap(find.text('Asian Games'));
    await tester.pumpAndSettle();
    expect(opened, [news, wiki]);
  });
}

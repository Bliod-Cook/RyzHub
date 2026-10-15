import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/prefs.dart';
import 'package:ryzhub/ui/code.dart';

import 'kernel.dart';

void main() {
  setUpAll(() => prefs.language = 'zh');

  testWidgets(
    'streaming Markdown supports a partial code block and independent copy',
    (tester) async {
      await tester.pumpWidget(
        MaterialApp(
          theme: hubTheme,
          home: Scaffold(
            body: messageCard(
              item: {
                'id': 'i',
                'kind': 'assistant',
                'state': 'streaming',
                'text': '中文 **Markdown**\n```rust\nfn main() {',
              },
            ),
          ),
        ),
      );
      expect(find.byTooltip('复制代码'), findsOneWidget);
      expect(find.text('生成中…'), findsOneWidget);
      expect(tester.takeException(), isNull);
    },
  );

  testWidgets('long code blocks open folded beside line numbers', (
    tester,
  ) async {
    Future<void> reply(
      int lines, {
      String state = 'completed',
    }) => tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
        home: Scaffold(
          body: ListView(
            children: [
              messageCard(
                item: {
                  'id': 'i',
                  'kind': 'assistant',
                  'state': state,
                  'text':
                      '```python\n${[for (var i = 1; i <= lines; i++) 'x = $i'].join('\n')}\n```',
                },
              ),
            ],
          ),
        ),
      ),
    );

    await reply(30);
    expect(
      find.text([for (var i = 1; i <= 30; i++) i].join('\n')),
      findsOneWidget,
    );
    expect(find.text('展开全部 · 共 30 行'), findsOneWidget);

    // Short code has nothing to fold.
    await tester.pumpWidget(const SizedBox());
    await reply(5, state: 'streaming');
    expect(find.byTooltip('折叠代码'), findsNothing);
    expect(find.byTooltip('展开代码'), findsNothing);
    // Code that grows long while it streams stays open, and can fold.
    await reply(30, state: 'streaming');
    expect(find.text('展开全部 · 共 30 行'), findsNothing);
    expect(find.byTooltip('折叠代码'), findsOneWidget);
    expect(tester.takeException(), isNull);
  });

  testWidgets('code is coloured by the kernel, an unknown language stays '
      'plain; diffs and terminal output show as such', (tester) async {
    const style = TextStyle(fontSize: 13);
    Future<SelectableText> shown(Widget widget) async {
      await tester.pumpWidget(
        MaterialApp(
          theme: hubTheme,
          home: Scaffold(body: widget),
        ),
      );
      return tester.widget<SelectableText>(find.byType(SelectableText));
    }

    Future<List<TextSpan>> spans(Widget widget) async {
      final text = await shown(widget);
      return [
        for (final span in text.textSpan!.children!.cast<TextSpan>())
          if (span.children != null)
            ...span.children!.cast<TextSpan>()
          else
            span,
      ];
    }

    final rust = await spans(
      const CodeText('fn main() {}', language: 'rust', style: style),
    );
    final keyword = rust.firstWhere((s) => s.text == 'fn');
    expect(keyword.style?.color, isNotNull, reason: 'a keyword has its colour');

    final unknown = await shown(
      const CodeText('{"limit": 10}', language: 'foobar', style: style),
    );
    expect(unknown.textSpan, isNull);
    expect(unknown.data, '{"limit": 10}');

    final diff = await spans(
      const DiffText('@@ -1 +1 @@\n-a\n+b', style: style),
    );
    expect(diff.map((s) => s.text).join(), '@@ -1 +1 @@\n-a\n+b');
    expect(
      diff[2].style?.backgroundColor,
      isNotNull,
      reason: 'an added line is tinted',
    );

    final ansi = await spans(
      const AnsiText('\x1b[31mred\x1b[0m plain', style: style),
    );
    expect(ansi.map((s) => s.text).join(), 'red plain');
    expect(ansi.first.style?.color, const Color(0xffcd3131));
  });
}

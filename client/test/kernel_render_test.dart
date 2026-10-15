import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/ui/code.dart';

import 'kernel.dart';

void main() {
  test('timeline diffs keep the blocks they do not replace', () async {
    final hub = HubController()
      ..org = 'o'
      ..user = 'u'
      ..selected = 'c';
    addTearDown(hub.dispose);
    Map<String, dynamic> paragraph(String text) => {
      't': 'paragraph',
      'inlines': [
        {'t': 'text', 'text': text},
      ],
    };
    Future<void> change(Map<String, dynamic> timeline) => hub.debugChanges(
      jsonEncode([
        {'channel': 'conv:c', 'actions': [], 'runs': [], 'timeline': timeline},
      ]),
    );
    await change({
      'rows': [
        {
          'op': 'reset',
          'rows': [
            {'id': 'a', 'item': 'a'},
          ],
        },
      ],
      'items': [
        {
          'id': 'a',
          'item': {
            'id': 'a',
            'kind': 'assistant',
            'state': 'streaming',
            'text': 'one\n\ntw',
            'first_seq': 1,
          },
          'blocks': {
            'from': 0,
            'blocks': [paragraph('one'), paragraph('tw')],
          },
          'chips': [],
        },
      ],
    });
    final first = hub.entry('a')!.blocks.first;
    await change({
      'rows': [
        {
          'op': 'insert',
          'index': 1,
          'row': {'id': 'b', 'item': 'b'},
        },
      ],
      'items': [
        {
          'id': 'a',
          'append': 'o',
          'blocks': {
            'from': 1,
            'blocks': [paragraph('two')],
          },
        },
        {
          'id': 'b',
          'item': {'id': 'b', 'kind': 'user', 'text': 'hi', 'first_seq': 2},
        },
      ],
    });
    final a = hub.entry('a')!;
    expect(a.item['text'], 'one\n\ntwo');
    expect(
      identical(a.blocks.first, first),
      isTrue,
      reason: 'kept, not rebuilt',
    );
    expect(a.blocks.last['inlines'][0]['text'], 'two');
    expect(hub.rows.map((r) => r['id']), ['a', 'b']);
    expect(hub.items.map((i) => i['id']), ['a', 'b']);
  });

  testWidgets(
    'code is coloured by the kernel; diffs and terminal output show as such',
    (tester) async {
      const style = TextStyle(fontSize: 13);
      Future<List<TextSpan>> spans(Widget widget) async {
        await tester.pumpWidget(MaterialApp(home: Scaffold(body: widget)));
        final text = tester.widget<SelectableText>(find.byType(SelectableText));
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
      expect(
        keyword.style?.color,
        isNotNull,
        reason: 'a keyword has its colour',
      );

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
    },
  );

  testWidgets('a reply renders its blocks: headings, lists, tables and code', (
    tester,
  ) async {
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: SingleChildScrollView(
            child: messageCard(
              item: {
                'id': 'r',
                'kind': 'assistant',
                'state': 'completed',
                'text':
                    '# Plan\n\n- [x] done\n- todo\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n'
                    '```python\nprint(1)\n```\n\n> quoted **bold**',
              },
            ),
          ),
        ),
      ),
    );
    expect(find.text('Plan'), findsOneWidget);
    expect(find.byIcon(Icons.check_box), findsOneWidget);
    expect(find.text('•'), findsOneWidget);
    expect(find.byType(Table), findsOneWidget);
    expect(find.byType(CodeBlock), findsOneWidget);
    expect(find.textContaining('quoted', findRichText: true), findsOneWidget);
    expect(tester.takeException(), isNull);
  });
}

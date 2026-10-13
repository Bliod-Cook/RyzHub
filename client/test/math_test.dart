import 'package:flutter/material.dart';
import 'package:flutter_math_fork/flutter_math.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/ui/messages.dart';

import 'kernel.dart';

void main() {
  Future<void> show(
    WidgetTester tester,
    String text, {
    String kind = 'assistant',
    String state = 'completed',
  }) => tester.pumpWidget(
    MaterialApp(
      home: Scaffold(
        body: ListView(
          children: [
            MessageCard(
              item: {'id': 'i', 'kind': kind, 'state': state, 'text': text},
            ),
          ],
        ),
      ),
    ),
  );

  /// The text the reply shows, a formula as U+FFFC.
  String shown(WidgetTester tester) => shownText(tester);

  testWidgets('every delimiter renders TeX', (tester) async {
    for (final text in [
      r'$$x = \frac{-b \pm \sqrt{b^2-4ac}}{2a}$$',
      r'\[E = mc^2\]',
      r'设 \(\alpha + \beta\) 为常数',
      r'勾股定理：$a^2+b^2=c^2$，其中 $c$ 是斜边。',
    ]) {
      await show(tester, text);
      expect(find.byType(Math), findsWidgets, reason: text);
      expect(shown(tester), isNot(contains(r'$')), reason: text);
      expect(shown(tester), isNot(contains(r'\')), reason: text);
      expect(tester.takeException(), isNull, reason: text);
    }
    expect(shown(tester), allOf(contains('勾股定理'), contains('是斜边')));
  });

  testWidgets('displayed formulas take a centred line of their own', (
    tester,
  ) async {
    await show(tester, r'前文 \[E = mc^2\] 后文');
    expect(find.byType(Math), findsOneWidget);
    final centre = tester.getCenter(find.byType(Math)).dx;
    expect(centre, closeTo(tester.getCenter(find.byType(ListView)).dx, 2));
    expect(shown(tester), allOf(contains('前文'), contains('后文')));

    // Lines of a formula that read as Markdown stay in it.
    await show(tester, '推导：\n\$\$\nx = a\n- b\n\n+ c\n\$\$\n结束');
    expect(find.byType(Math), findsOneWidget);
    expect(find.text('•'), findsNothing);
    expect(shown(tester), allOf(contains('推导：'), contains('结束')));
    expect(tester.takeException(), isNull);
  });

  testWidgets('formulas render in list items and table cells', (tester) async {
    await show(
      tester,
      '1. 求根：\n   \$\$\n   x = \\frac{-b}{2a}\n   \$\$\n2. 代入',
    );
    expect(find.byType(Math), findsOneWidget);
    expect(find.text('1.'), findsOneWidget);
    expect(find.text('2.'), findsOneWidget);

    await show(
      tester,
      '| 函数 | 导数 |\n|---|---|\n| \$x^2\$ | \$\\frac{1}{2}x_1\$ |',
    );
    expect(find.byType(Math), findsWidgets);
    expect(tester.takeException(), isNull);
  });

  testWidgets('subscripts are not emphasis', (tester) async {
    await show(tester, r'$a_1 + b_2$ 与 $c_1$');
    expect(find.byType(Math), findsWidgets);
    expect(shown(tester), isNot(contains('_')));
    expect(shown(tester), contains('与'));
  });

  testWidgets('prices, escapes and code stay text', (tester) async {
    for (final (text, kept) in [
      (r'它要 $5，那个要 $10', r'$5，那个要 $10'),
      (r'costs $5 and $10', r'$5 and $10'),
      (r'literal \$x\$ here', r'$x$'),
      (r'inline `$x$` code', r'$x$'),
      ('```\n\$x\$\n```', r'$x$'),
    ]) {
      await show(tester, text);
      expect(find.byType(Math), findsNothing, reason: text);
      expect(
        find.textContaining(kept, findRichText: true).evaluate().isNotEmpty ||
            shown(tester).contains(kept),
        isTrue,
        reason: text,
      );
    }
  });

  testWidgets('TeX that does not parse shows its source', (tester) async {
    await show(tester, r'坏的 $\frac{$ 公式');
    expect(find.text(r'$\frac{$'), findsOneWidget);
    expect(tester.takeException(), isNull);

    // A formula still streaming shows as written until it closes.
    await show(tester, '答案：\n\$\$\nx = \\frac{1', state: 'streaming');
    expect(find.byType(Math), findsNothing);
    expect(shown(tester), contains(r'$$'));
    expect(tester.takeException(), isNull);
  });

  testWidgets('user bubbles render formulas', (tester) async {
    await show(tester, r'请解释 $$\int_0^1 x\,dx$$ 的值', kind: 'user');
    expect(find.byType(Math), findsOneWidget);
    expect(tester.takeException(), isNull);
  });
}

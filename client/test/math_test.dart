import 'package:flutter/material.dart';
import 'package:flutter_math_fork/flutter_math.dart';
import 'package:flutter_test/flutter_test.dart';

import 'kernel.dart';

void main() {
  Future<void> show(
    WidgetTester tester,
    String text, {
    String kind = 'assistant',
    String state = 'completed',
  }) => tester.pumpWidget(
    MaterialApp(
      theme: hubTheme,
      home: Scaffold(
        body: ListView(
          children: [
            messageCard(
              item: {'id': 'i', 'kind': kind, 'state': state, 'text': text},
            ),
          ],
        ),
      ),
    ),
  );

  testWidgets(
    'a displayed formula renders as TeX on a centred line of its own',
    (tester) async {
      await show(tester, r'前文 \[E = mc^2\] 后文');
      expect(find.byType(Math), findsOneWidget);
      final centre = tester.getCenter(find.byType(Math)).dx;
      expect(centre, closeTo(tester.getCenter(find.byType(ListView)).dx, 2));
      expect(shownText(tester), allOf(contains('前文'), contains('后文')));
      expect(tester.takeException(), isNull);
    },
  );

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

  testWidgets('TeX that does not parse shows its source', (tester) async {
    await show(tester, r'坏的 $\frac{$ 公式');
    expect(find.text(r'$\frac{$'), findsOneWidget);
    expect(tester.takeException(), isNull);

    // A formula still streaming shows as written until it closes.
    await show(tester, '答案：\n\$\$\nx = \\frac{1', state: 'streaming');
    expect(find.byType(Math), findsNothing);
    expect(shownText(tester), contains(r'$$'));
    expect(tester.takeException(), isNull);
  });

  testWidgets('user bubbles render formulas', (tester) async {
    await show(tester, r'请解释 $$\int_0^1 x\,dx$$ 的值', kind: 'user');
    expect(find.byType(Math), findsOneWidget);
    expect(tester.takeException(), isNull);
  });
}

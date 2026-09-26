import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/main.dart';

void main() {
  testWidgets('Chinese composition is not submitted by Ctrl+Enter', (
    tester,
  ) async {
    final sent = <String>[];
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: Composer(
            onSend: (text) async {
              sent.add(text);
            },
            onCancel: () {},
            running: false,
          ),
        ),
      ),
    );
    await tester.showKeyboard(find.byType(TextField));
    tester.testTextInput.updateEditingValue(
      const TextEditingValue(
        text: 'zhong',
        selection: TextSelection.collapsed(offset: 5),
        composing: TextRange(start: 0, end: 5),
      ),
    );
    await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
    await tester.pump();
    expect(sent, isEmpty);
    tester.testTextInput.updateEditingValue(
      const TextEditingValue(
        text: '中文输入',
        selection: TextSelection.collapsed(offset: 4),
      ),
    );
    await tester.tap(find.byTooltip('发送'));
    await tester.pump();
    expect(sent, ['中文输入']);
  });
  testWidgets(
    'streaming Markdown supports a partial code block and independent copy',
    (tester) async {
      await tester.pumpWidget(
        const MaterialApp(
          home: Scaffold(
            body: MessageCard(
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
  testWidgets('ten thousand messages remain lazily built', (tester) async {
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: ListView.builder(
            itemCount: 10000,
            itemBuilder: (_, i) => MessageCard(
              item: {
                'id': '$i',
                'kind': 'assistant',
                'state': 'completed',
                'text': '消息 $i：中文 / Rust code',
              },
            ),
          ),
        ),
      ),
    );
    expect(find.byType(MessageCard).evaluate().length, lessThan(20));
    await tester.drag(find.byType(ListView), const Offset(0, -2000));
    await tester.pump();
    expect(tester.takeException(), isNull);
  });

  testWidgets('question prevents composed text and duplicate pending answers', (
    tester,
  ) async {
    final answers = <String>[];
    final pending = Completer<void>();
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: QuestionCard(
            summary: '你希望使用哪种颜色？',
            onAnswer: (text) async {
              answers.add(text);
              await pending.future;
            },
          ),
        ),
      ),
    );
    await tester.showKeyboard(find.byType(TextField));
    tester.testTextInput.updateEditingValue(
      const TextEditingValue(
        text: 'lv',
        selection: TextSelection.collapsed(offset: 2),
        composing: TextRange(start: 0, end: 2),
      ),
    );
    await tester.tap(find.text('提交回答'));
    await tester.pump();
    expect(answers, isEmpty);
    await tester.enterText(find.byType(TextField), '绿色');
    await tester.tap(find.text('提交回答'));
    await tester.pump();
    await tester.tap(find.text('提交回答'));
    expect(answers, ['绿色']);
    pending.complete();
    await tester.pump();
  });

  testWidgets('side query and active-run supplement route independently', (
    tester,
  ) async {
    final mainline = <String>[], side = <String>[];
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: Composer(
            onSend: (text) async {
              mainline.add(text);
            },
            onSideQuery: (text) async {
              side.add(text);
            },
            onCancel: () {},
            running: true,
            steer: true,
          ),
        ),
      ),
    );
    expect(find.text('补充指令将在下一次模型请求前采用'), findsOneWidget);
    await tester.tap(find.text('只读旁问'));
    await tester.enterText(find.byType(TextField), '旁问');
    await tester.tap(find.byTooltip('发送'));
    await tester.pump();
    expect(side, ['旁问']);
    expect(mainline, isEmpty);
    await tester.tap(find.text('只读旁问'));
    await tester.enterText(find.byType(TextField), '补充');
    await tester.tap(find.byTooltip('发送'));
    await tester.pump();
    expect(mainline, ['补充']);
    expect(find.byTooltip('停止'), findsOneWidget);
  });

  testWidgets('native question shows readable choices and sends plain answer', (
    tester,
  ) async {
    final answers = <String>[];
    final summary = jsonEncode([
      {
        'id': 'color',
        'question': '你希望使用哪种颜色？',
        'options': [
          {'label': '蓝色', 'description': 'Blue'},
          {'label': '绿色', 'description': 'Green'},
        ],
      },
    ]);
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: QuestionCard(
            summary: summary,
            onAnswer: (text) async => answers.add(text),
          ),
        ),
      ),
    );
    expect(find.text(summary), findsNothing);
    expect(find.text('你希望使用哪种颜色？'), findsOneWidget);
    await tester.tap(find.text('蓝色'));
    await tester.tap(find.text('提交回答'));
    await tester.pump();
    expect(answers, ['蓝色']);
  });

  testWidgets('native question group requires every answer and preserves IDs', (
    tester,
  ) async {
    final answers = <String>[];
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: QuestionCard(
            summary: jsonEncode([
              {'id': 'color', 'question': '选择颜色'},
              {'id': 'language', 'question': '选择语言'},
            ]),
            onAnswer: (text) async => answers.add(text),
          ),
        ),
      ),
    );
    await tester.enterText(find.byType(TextField).first, '蓝色');
    await tester.tap(find.text('提交回答'));
    expect(answers, isEmpty);
    await tester.ensureVisible(find.byType(TextField).last);
    await tester.enterText(find.byType(TextField).last, '中文');
    await tester.tap(find.text('提交回答'));
    await tester.pump();
    expect(jsonDecode(answers.single), {'color': '蓝色', 'language': '中文'});
    expect(tester.takeException(), isNull);
  });
}

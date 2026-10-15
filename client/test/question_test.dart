import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/prefs.dart';
import 'package:ryzhub/ui/messages.dart';

import 'kernel.dart';

void main() {
  setUpAll(() => prefs.language = 'zh');

  testWidgets('question prevents composed text and duplicate pending answers', (
    tester,
  ) async {
    final answers = <String>[];
    final pending = Completer<void>();
    await tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
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
        theme: hubTheme,
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
        theme: hubTheme,
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

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/prefs.dart';
import 'package:ryzhub/ui/composer.dart';

import 'kernel.dart';

Future<void> _composer(
  WidgetTester tester,
  Future<void> Function(String) onSend,
) => tester.pumpWidget(
  MaterialApp(
    theme: hubTheme,
    home: Scaffold(
      body: Composer(onSend: onSend, onCancel: () {}, running: false),
    ),
  ),
);

void main() {
  setUpAll(() => prefs.language = 'en');

  testWidgets('Enter sends, Shift+Enter breaks the line, IME keeps Enter', (
    tester,
  ) async {
    final sent = <String>[];
    final focus = FocusNode();
    addTearDown(focus.dispose);
    await tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
        home: Scaffold(
          body: Composer(
            onSend: (text) async {
              sent.add(text);
            },
            onCancel: () {},
            running: false,
            focusNode: focus,
          ),
        ),
      ),
    );
    // A caller's focus lands in the message field.
    focus.requestFocus();
    await tester.pump();
    expect(
      tester.widget<EditableText>(find.byType(EditableText)).focusNode,
      focus,
    );
    expect(focus.hasFocus, isTrue);
    tester.testTextInput.updateEditingValue(
      const TextEditingValue(
        text: 'zhong',
        selection: TextSelection.collapsed(offset: 5),
        composing: TextRange(start: 0, end: 5),
      ),
    );
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
    await tester.pump();
    expect(sent, isEmpty);
    tester.testTextInput.updateEditingValue(
      const TextEditingValue(
        text: '第一行',
        selection: TextSelection.collapsed(offset: 3),
      ),
    );
    await tester.sendKeyDownEvent(LogicalKeyboardKey.shiftLeft);
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.shiftLeft);
    await tester.pump();
    expect(sent, isEmpty);
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    await tester.pump();
    expect(sent, ['第一行']);
  });

  testWidgets('a message sent while the run goes steers it', (tester) async {
    final mainline = <String>[];
    await tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
        home: Scaffold(
          body: Composer(
            onSend: (text) async {
              mainline.add(text);
            },
            onCancel: () {},
            running: true,
            steer: true,
          ),
        ),
      ),
    );
    await tester.enterText(find.byType(TextField), '补充');
    await tester.tap(find.byTooltip('Send'));
    await tester.pump();
    expect(mainline, ['补充']);
    expect(find.byTooltip('Stop'), findsOneWidget);
  });

  testWidgets('composer stays on one row until the text reaches the controls', (
    tester,
  ) async {
    await tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
        home: Scaffold(
          body: Center(
            child: SizedBox(
              width: 600,
              child: Composer(
                onSend: (_) async {},
                onCancel: () {},
                running: false,
                leading: const Icon(Icons.add),
                trailing: const Text('gpt-5.4'),
              ),
            ),
          ),
        ),
      ),
    );
    double y(Finder f) => tester.getCenter(f).dy;
    final field = find.byType(TextField), send = find.byTooltip('Send');
    expect((y(field) - y(send)).abs(), lessThan(1));
    await tester.enterText(field, '短句');
    await tester.pump();
    expect((y(field) - y(send)).abs(), lessThan(1));
    // Long text takes its own row; the field keeps focus through the switch.
    await tester.enterText(field, '很长的一句话' * 20);
    await tester.pump();
    expect(y(send), greaterThan(y(field) + 10));
    final editable = tester.state<EditableTextState>(find.byType(EditableText));
    expect(editable.widget.focusNode.hasFocus, isTrue);
    await tester.enterText(field, '第一行\n第二行');
    await tester.pump();
    expect(y(send), greaterThan(y(field) + 10));
    await tester.enterText(field, '');
    await tester.pump();
    expect((y(field) - y(send)).abs(), lessThan(1));
  });

  testWidgets('what is typed while a message goes out stays', (tester) async {
    final sending = Completer<void>();
    final sent = <String>[];
    await _composer(tester, (text) {
      sent.add(text);
      return sending.future;
    });
    await tester.enterText(find.byType(TextField), 'first message');
    await tester.tap(find.byTooltip('Send'));
    await tester.pump();
    expect(sent, ['first message']);
    // The field is free at once for the next draft.
    expect(find.text('first message'), findsNothing);
    await tester.enterText(find.byType(TextField), 'next draft');
    sending.complete();
    await tester.pumpAndSettle();
    expect(find.text('next draft'), findsOneWidget);
  });

  testWidgets(
    'a message that fails to go out comes back before the new draft',
    (tester) async {
      final sending = Completer<void>();
      await _composer(tester, (_) => sending.future);
      await tester.enterText(find.byType(TextField), 'first message');
      await tester.tap(find.byTooltip('Send'));
      await tester.pump();
      await tester.enterText(find.byType(TextField), 'next draft');
      sending.completeError(Exception('offline'));
      await tester.pumpAndSettle();
      expect(find.text('first message\nnext draft'), findsOneWidget);
    },
  );

  testWidgets(
    'the composer sends files without text, but not while uploading',
    (tester) async {
      final sent = <String>[];
      Future<void> pump({required bool ready, required bool blocked}) =>
          tester.pumpWidget(
            MaterialApp(
              theme: hubTheme,
              home: Scaffold(
                body: Composer(
                  onSend: (text) async => sent.add(text),
                  onCancel: () {},
                  running: false,
                  canSendEmpty: ready,
                  blocked: blocked,
                  attachments: const Text('chips'),
                ),
              ),
            ),
          );
      await pump(ready: true, blocked: true);
      expect(find.text('chips'), findsOneWidget);
      await tester.tap(find.byTooltip('Send'));
      await tester.pump();
      expect(sent, isEmpty);
      await pump(ready: true, blocked: false);
      await tester.tap(find.byTooltip('Send'));
      await tester.pump();
      expect(sent, ['']);
      await pump(ready: false, blocked: false);
      await tester.tap(find.byTooltip('Send'));
      await tester.pump();
      expect(sent, ['']);
    },
  );

  testWidgets(
    'typing / offers commands by group; keys pick one before sending',
    (tester) async {
      final ran = <String>[], sent = <String>[];
      final text = TextEditingController();
      await tester.pumpWidget(
        MaterialApp(
          theme: hubTheme,
          home: Scaffold(
            body: Align(
              alignment: Alignment.bottomCenter,
              child: Composer(
                controller: text,
                onSend: (t) async => sent.add(t),
                onCancel: () {},
                running: false,
                commands: [
                  SlashCommand(
                    'compact',
                    'Compact',
                    'Commands',
                    run: () => ran.add('compact'),
                  ),
                  SlashCommand(
                    'new',
                    'New chat',
                    'Conversation',
                    run: () => ran.add('new'),
                  ),
                  const SlashCommand('review', 'Review changes', 'Agent'),
                ],
              ),
            ),
          ),
        ),
      );
      await tester.tap(find.byType(TextField));
      await tester.enterText(find.byType(TextField), '/');
      await tester.pump();
      expect(find.text('/compact'), findsOneWidget);
      expect(find.text('Agent'), findsOneWidget);
      // Down to `/new`, then Enter runs it instead of sending.
      await tester.sendKeyEvent(LogicalKeyboardKey.arrowDown);
      await tester.sendKeyEvent(LogicalKeyboardKey.enter);
      await tester.pump();
      expect(ran, ['new']);
      expect(sent, isEmpty);
      expect(text.text, isEmpty);
      // A harness command takes arguments: picking it fills the field.
      await tester.enterText(find.byType(TextField), '/re');
      await tester.pump();
      expect(find.text('/compact'), findsNothing);
      await tester.sendKeyEvent(LogicalKeyboardKey.tab);
      await tester.pump();
      expect(text.text, '/review ');
      expect(find.text('/review'), findsNothing, reason: 'arguments follow');
      // Escape leaves the text to send as typed.
      await tester.enterText(find.byType(TextField), '/co');
      await tester.pump();
      await tester.sendKeyEvent(LogicalKeyboardKey.escape);
      await tester.pump();
      expect(find.text('/compact'), findsNothing);
      await tester.sendKeyEvent(LogicalKeyboardKey.enter);
      await tester.pump();
      expect(sent, ['/co']);
    },
  );

  testWidgets('the approval mode menu explains each mode and picks one', (
    tester,
  ) async {
    final picked = <String>[];
    await tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
        home: Scaffold(
          body: Center(
            child: ApprovalMenu(
              value: 'ask',
              modes: const ['ask', 'allow_edits'],
              onSelected: picked.add,
            ),
          ),
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
}

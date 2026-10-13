import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/ui/composer.dart';

void main() {
  testWidgets(
    'typing / offers commands by group; keys pick one before sending',
    (tester) async {
      final ran = <String>[], sent = <String>[];
      final text = TextEditingController();
      await tester.pumpWidget(
        MaterialApp(
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
}

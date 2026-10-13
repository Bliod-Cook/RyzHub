import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/main.dart';
import 'package:ryzhub/theme.dart';
import 'package:ryzhub/ui/terminal.dart';

/// A hub whose ticket requests fail, as with the server unreachable.
class _Hub extends HubController {
  int tickets = 0;
  _Hub() {
    org = 'org';
  }

  @override
  Future<dynamic> request(
    String path, {
    Map<String, dynamic>? body,
    String? method,
    Duration timeout = const Duration(seconds: 15),
  }) async {
    tickets++;
    throw StateError('offline');
  }
}

void main() {
  setUpAll(() => prefs.language = 'en');

  testWidgets('a lost terminal connection retries until the panel closes', (
    tester,
  ) async {
    final hub = _Hub();
    var closed = false;
    await tester.pumpWidget(
      MaterialApp(
        theme: buildTheme(Brightness.light, accents.values.first),
        home: Scaffold(
          body: TerminalPanel(
            hub: hub,
            conversation: 'a',
            onClose: () => closed = true,
          ),
        ),
      ),
    );
    await tester.pump();
    expect(find.text('Disconnected, reconnecting…'), findsOneWidget);
    expect(hub.tickets, 1);
    // Retries back off; the session is detached, not ended.
    await tester.pump(const Duration(milliseconds: 600));
    expect(hub.tickets, 2);
    expect(find.text('End session'), findsOneWidget);
    await tester.tap(
      find.byTooltip('Hide terminal (the session keeps running)'),
    );
    expect(closed, isTrue);
    // Disposing stops retrying.
    await tester.pumpWidget(const SizedBox());
    await tester.pump(const Duration(seconds: 5));
    expect(hub.tickets, 2);
  });
}

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/prefs.dart';
import 'package:ryzhub/theme.dart';
import 'package:ryzhub/ui/terminal.dart';

/// A hub whose ticket requests fail with [failure].
class _Hub extends HubController {
  final Object failure;
  int tickets = 0;
  _Hub(this.failure) {
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
    throw failure;
  }
}

Future<void> _show(WidgetTester tester, _Hub hub, VoidCallback onClose) =>
    tester.pumpWidget(
      MaterialApp(
        theme: buildTheme(Brightness.light, accents.values.first),
        home: Scaffold(
          body: TerminalPanel(hub: hub, conversation: 'a', onClose: onClose),
        ),
      ),
    );

void main() {
  setUpAll(() => prefs.language = 'en');

  testWidgets('a lost terminal connection retries until the panel closes', (
    tester,
  ) async {
    final hub = _Hub(http.ClientException('offline'));
    var closed = false;
    await _show(tester, hub, () => closed = true);
    await tester.pump();
    expect(find.text('Disconnected, reconnecting…'), findsOneWidget);
    expect(hub.tickets, 1);
    // Retries back off; the session is detached, not ended.
    await tester.pump(const Duration(milliseconds: 600));
    expect(hub.tickets, greaterThan(1));
    expect(find.text('End session'), findsOneWidget);
    await tester.tap(find.byTooltip('Hide terminal'));
    expect(closed, isTrue);
    // Disposing stops retrying.
    final tried = hub.tickets;
    await tester.pumpWidget(const SizedBox());
    await tester.pump(const Duration(seconds: 5));
    expect(hub.tickets, tried);
  });

  testWidgets('a refused terminal says why instead of retrying', (
    tester,
  ) async {
    final hub = _Hub(Exception('UNAUTHORIZED'));
    await _show(tester, hub, () {});
    await tester.pump();
    expect(find.text('Your session expired. Sign in again.'), findsOneWidget);
    await tester.pump(const Duration(seconds: 5));
    expect(hub.tickets, 1);
  });
}

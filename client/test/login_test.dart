import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:ryzhub/prefs.dart';
import 'package:ryzhub/ui/login.dart';

import 'kernel.dart';

void main() {
  setUpAll(() => prefs.language = 'en');

  testWidgets(
    'a server closed to sign-up opens on log in without a sign-up switch',
    (tester) async {
      final server = MockClient(
        (_) async =>
            http.Response('{"registration":false,"auth_providers":[]}', 200),
      );
      await http.runWithClient(() async {
        await tester.pumpWidget(
          MaterialApp(
            theme: hubTheme,
            home: LoginPage(onLogin: (_, _, _, _) async {}),
          ),
        );
        await tester.pump();
      }, () => server);

      expect(find.widgetWithText(FilledButton, 'Log in'), findsOneWidget);
      expect(find.text('Already have an account? Log in'), findsNothing);
      expect(find.text('Create a new account'), findsNothing);
    },
  );
}

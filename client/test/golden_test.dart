// Pixel comparisons of the timeline and cards against `test/goldens/*.png`,
// rendered with the bundled fonts. The images are made on Linux; regenerate
// them after an intended visual change with
// `flutter test --update-goldens test/golden_test.dart`.
import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/prefs.dart';
import 'package:ryzhub/theme.dart';
import 'package:ryzhub/ui/messages.dart';
import 'package:ryzhub/ui/plugin_ui.dart';

import 'kernel.dart';

Future<void> _font(String family, String path) async {
  final bytes = await File(path).readAsBytes();
  await (FontLoader(
    family,
  )..addFont(Future.value(ByteData.view(bytes.buffer)))).load();
}

Future<void> _fonts() async {
  await _font('RyzHub Sans', 'assets/fonts/RyzHubSans.ttf');
  await _font('0xProto', 'assets/fonts/0xProto-Regular-NL.ttf');
  // Code is set in `monospace`, which falls back to the UI font in tests.
  await _font('monospace', 'assets/fonts/0xProto-Regular-NL.ttf');
  final flutter = Platform.environment['FLUTTER_ROOT'];
  if (flutter != null) {
    await _font(
      'MaterialIcons',
      '$flutter/bin/cache/artifacts/material_fonts/MaterialIcons-Regular.otf',
    );
  }
}

Widget _frame(Brightness brightness, Widget child) => MaterialApp(
  debugShowCheckedModeBanner: false,
  theme: buildTheme(brightness, accentColor('orange')),
  home: Scaffold(
    body: Center(
      child: SizedBox(
        width: 720,
        child: Padding(padding: const EdgeInsets.all(Space.s16), child: child),
      ),
    ),
  ),
);

Widget _timeline() {
  final fixture = jsonDecode(
    File('test/goldens/timeline.json').readAsStringSync(),
  );
  final rows = timeline([
    for (final item in fixture['items'] as List)
      Map<String, dynamic>.from(item),
  ]);
  return ListView(
    children: [
      for (final row in rows)
        Padding(
          padding: const EdgeInsets.only(bottom: Space.s12),
          child: row['kind'] == 'steps'
              ? stepsGroup(
                  steps: row['steps'],
                  running: true,
                  latest: row['latest'] == true,
                )
              : messageCard(item: row, agentLabel: 'Codex'),
        ),
    ],
  );
}

Widget _cards() => Column(
  crossAxisAlignment: CrossAxisAlignment.stretch,
  children: [
    ApprovalCard(
      summary: 'Run `rm -rf build/` in /workspace?',
      onDeny: () {},
      onAllow: () {},
    ),
    const SizedBox(height: Space.s16),
    PluginComponent(
      component: const {
        'type': 'card',
        'title': 'Open tickets',
        'children': [
          {
            'type': 'table',
            'columns': ['Id', 'Title', 'State'],
            'rows': [
              ['7', 'Login redirect loops', 'open'],
              ['8', '登录页样式错位', 'closed'],
            ],
          },
          {
            'type': 'button',
            'label': 'Close #7',
            'action': {
              'tool': 'close',
              'arguments': {'id': '7'},
            },
          },
        ],
      },
      call: (_, _) async => const {},
    ),
  ],
);

void main() {
  setUpAll(_fonts);
  setUp(() => prefs.language = 'zh');

  for (final brightness in Brightness.values) {
    testWidgets('timeline · ${brightness.name}', (tester) async {
      tester.view.physicalSize = const Size(720, 1100);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      await tester.pumpWidget(_frame(brightness, _timeline()));
      // The running step spins forever: step a fixed time instead of settling.
      await tester.pump(const Duration(seconds: 1));
      // Open the diff the edit step folds away.
      await tester.tap(find.text('修改 src/login.rs'));
      for (var frame = 0; frame < 20; frame++) {
        await tester.pump(const Duration(milliseconds: 50));
      }
      await expectLater(
        find.byType(ListView),
        matchesGoldenFile('goldens/timeline_${brightness.name}.png'),
      );
    });
  }

  testWidgets('approval and plugin cards', (tester) async {
    tester.view.physicalSize = const Size(720, 520);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.reset);
    await tester.pumpWidget(_frame(Brightness.light, _cards()));
    await tester.pumpAndSettle();
    await expectLater(
      find.byType(Column).first,
      matchesGoldenFile('goldens/cards.png'),
    );
  });
}

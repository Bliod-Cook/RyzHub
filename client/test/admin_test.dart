import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/prefs.dart';
import 'package:ryzhub/ui/admin/fields.dart';
import 'package:ryzhub/ui/admin/overview.dart';
import 'package:ryzhub/ui/common.dart';
import 'package:ryzhub/ui/settings.dart';

import 'kernel.dart';

/// A hub whose requests get their answer by path, or throw an error code.
class _Hub extends HubController {
  _Hub(this.answers);
  final Map<String, Object> answers;
  final asked = <String>[];

  @override
  Future<dynamic> request(
    String path, {
    Map<String, dynamic>? body,
    String? method,
    Duration timeout = const Duration(seconds: 15),
  }) async {
    asked.add(path);
    // Answers come after the next frame, as over a network.
    await Future<void>.delayed(const Duration(milliseconds: 1));
    final answer = answers[path.split('?').first];
    if (answer is Exception) throw answer;
    return answer;
  }
}

class _Actions implements HubActions {
  @override
  dynamic noSuchMethod(Invocation invocation) => null;
}

/// A week's overview with nothing in it yet.
Map<String, dynamic> _empty() => {
  'first_day': '2026-10-22',
  'users': {'total': 1, 'disabled': 0, 'deletion_pending': 0},
  'model_users': {'week': 0, 'month': 0},
  'conversations': {'chat': 0, 'work': 0},
  'runs': {'running': 0, 'requires_action': 0},
  'sandboxes_running': 0,
  'node_capacity': null,
  'nodes': {},
  'checkpoint_bytes': 0,
  'attachment_bytes': 0,
  'daily': [
    for (var d = 22; d <= 28; d++)
      {
        'day': '2026-10-$d',
        'input_tokens': 0,
        'output_tokens': 0,
        'model_users': 0,
        'chat_runs': 0,
        'work_runs': 0,
        'new_users': 0,
      },
  ],
  'models': [],
  'harnesses': [],
  'run_outcomes': {'completed': 0, 'failed': 0, 'cancelled': 0},
  'request_statuses': {'completed': 0, 'interrupted': 0, 'upstream_error': 0},
  'top_users': [],
  'estimated_tokens': 0,
  'total_tokens': 0,
  'retention': {'usage_days': 0, 'audit_days': 400},
};

Future<void> _wide(WidgetTester tester) async {
  tester.view.physicalSize = const Size(1400, 1000);
  tester.view.devicePixelRatio = 1;
  addTearDown(tester.view.reset);
}

void main() {
  setUp(() => prefs.language = 'en');

  testWidgets('only instance admins reach the panel, until it refuses them', (
    tester,
  ) async {
    await _wide(tester);
    final member = _Hub({});
    addTearDown(member.dispose);
    await tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
        home: SettingsPage(hub: member, app: _Actions()),
      ),
    );
    expect(find.text('Instance admin'), findsNothing);

    final admin = _Hub({'/admin/stats': _empty()})..isInstanceAdmin = true;
    addTearDown(admin.dispose);
    await tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
        home: SettingsPage(hub: admin, app: _Actions()),
      ),
    );
    await tester.tap(find.text('Instance admin'));
    await tester.pumpAndSettle();
    expect(find.widgetWithText(NavRow, 'Users'), findsOneWidget);
    expect(admin.asked.last, startsWith('/admin/stats?days=7&tz_offset='));
    // The server no longer takes the user as an admin: back to settings,
    // without the entry.
    admin.answers['/admin/stats'] = Exception('FORBIDDEN');
    await tester.tap(find.byTooltip('Refresh'));
    await tester.pump();
    await tester.pumpAndSettle();
    expect(find.widgetWithText(NavRow, 'Users'), findsNothing);
    expect(find.text('Instance admin'), findsNothing);
  });

  testWidgets('the overview renders an empty instance and a busy one', (
    tester,
  ) async {
    await _wide(tester);
    final busy = _empty()
      ..['users'] = {'total': 12, 'disabled': 2, 'deletion_pending': 1}
      ..['node_capacity'] = 8
      ..['nodes'] = {'active': 2}
      ..['checkpoint_bytes'] = 3 << 30
      ..['models'] = [
        {'model': 'claude-opus-5', 'input_tokens': 9000, 'output_tokens': 800},
      ]
      ..['harnesses'] = [
        {'harness': 'chat', 'runs': 30},
      ]
      ..['run_outcomes'] = {'completed': 25, 'failed': 4, 'cancelled': 1}
      ..['top_users'] = [
        {
          'user_id': 'u',
          'email': 'heavy@example.test',
          'input_tokens': 9000,
          'output_tokens': 800,
        },
      ]
      ..['estimated_tokens'] = 980
      ..['total_tokens'] = 9800;
    (busy['daily'] as List).last
      ..['input_tokens'] = 9000
      ..['chat_runs'] = 30
      ..['new_users'] = 3;
    for (final stats in [_empty(), busy]) {
      final hub = _Hub({'/admin/stats': stats})..isInstanceAdmin = true;
      addTearDown(hub.dispose);
      await tester.pumpWidget(
        MaterialApp(
          theme: hubTheme,
          home: Scaffold(
            body: SingleChildScrollView(
              child: Overview(key: ObjectKey(stats), hub: hub),
            ),
          ),
        ),
      );
      await tester.pumpAndSettle();
      expect(find.text('Users using models'), findsOneWidget);
      expect(tester.takeException(), isNull);
    }
    expect(find.text('heavy@example.test'), findsOneWidget);
    expect(find.text('3.0 GiB'), findsOneWidget);
    expect(find.text('10% estimated'), findsOneWidget);
  });

  test('numbers show in their largest whole unit and read back in bytes or seconds', () {
    String shown(List<Scale> scales, int value) {
      final (scale, n) = fit(scales, value);
      return '$n ${scale.id}';
    }

    expect(shown(byteScales, 10 << 30), '10 GiB');
    expect(shown(byteScales, 1536 << 20), '1536 MiB');
    expect(shown(byteScales, 512 << 10), '0.5 MiB');
    expect(shown(secondScales, 86400), '1 d');
    expect(shown(secondScales, 1800), '30 min');
    expect(shown(secondScales, 90), '90 s');
    expect(shown(secondScales, 0), '0 s');
    expect(unscale(byteScales[1], '1.5'), 3 << 29);
    expect(unscale(secondScales[3], '7'), 7 * 86400);
    expect(unscale(secondScales[0], '-1'), isNull);
    expect(unscale(secondScales[0], 'soon'), isNull);
  });
}

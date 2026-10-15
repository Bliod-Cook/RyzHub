import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/ui/plugin_app_bridge.dart';
import 'package:ryzhub/ui/plugin_ui.dart';
import 'package:ryzhub/ui/plugins.dart';

import 'kernel.dart';

Widget _page(Widget child) => MaterialApp(
  theme: hubTheme,
  home: Scaffold(body: SingleChildScrollView(child: child)),
);

void main() {
  testWidgets('a plugin question is a form whose answer is JSON', (
    tester,
  ) async {
    final answers = <String>[];
    await tester.pumpWidget(
      _page(
        ElicitationCard(
          summary: 'acme: Which project?',
          form: {
            'mode': 'form',
            'requestedSchema': {
              'type': 'object',
              'properties': {
                'project': {'type': 'string', 'title': 'Project'},
                'urgent': {'type': 'boolean', 'title': 'Urgent'},
              },
              'required': ['project'],
            },
          },
          onAnswer: (answer) async => answers.add(answer),
        ),
      ),
    );
    // A required field must be filled first.
    await tester.tap(find.text('Submit'));
    await tester.pump();
    expect(answers, isEmpty);
    await tester.enterText(
      find.widgetWithText(TextField, 'Project *'),
      'alpha',
    );
    await tester.tap(find.byType(Switch));
    await tester.tap(find.text('Submit'));
    await tester.pump();
    expect(jsonDecode(answers.single), {'project': 'alpha', 'urgent': true});
    await tester.tap(find.text('Decline'));
    await tester.pump();
    expect(answers.last, 'decline');
  });

  test('the app bridge gives the page its call and checks the rest', () async {
    final called = <String>[], said = <String>[];
    var height = 0.0;
    final bridge = AppBridge(
      toolInput: {'id': '7'},
      toolResult: {'structuredContent': 1},
      dark: false,
      callTool: (tool, arguments) async {
        called.add(tool);
        return {'ok': true};
      },
      sendMessage: (text) async {
        said.add(text);
        return text != 'refused';
      },
      openLink: (url) async => url.startsWith('https://'),
      resize: (value) => height = value,
    );
    final init = await bridge.handle({
      'jsonrpc': '2.0',
      'id': 1,
      'method': 'ui/initialize',
      'params': {},
    });
    expect(init.single['result']['hostContext']['theme'], 'light');
    final notes = await bridge.handle({
      'jsonrpc': '2.0',
      'method': 'ui/notifications/initialized',
    });
    expect(notes.map((n) => n['method']), [
      'ui/notifications/tool-input',
      'ui/notifications/tool-result',
    ]);
    expect(notes.first['params']['arguments'], {'id': '7'});
    final call = await bridge.handle({
      'id': 2,
      'method': 'tools/call',
      'params': {'name': 'lookup', 'arguments': {}},
    });
    expect(called.single, 'lookup');
    expect(call.single['result'], {'ok': true});
    final message = await bridge.handle({
      'id': 3,
      'method': 'ui/message',
      'params': {
        'content': [
          {'type': 'text', 'text': 'refused'},
        ],
      },
    });
    expect(said.single, 'refused');
    expect(message.single['error'], isNotNull);
    final link = await bridge.handle({
      'id': 4,
      'method': 'ui/open-link',
      'params': {'url': 'javascript:alert(1)'},
    });
    expect(link.single['error'], isNotNull);
    await bridge.handle({
      'method': 'ui/notifications/size-changed',
      'params': {'height': 420},
    });
    expect(height, 420);
    final other = await bridge.handle({'id': 5, 'method': 'conversation/read'});
    expect(other.single['error']['message'], 'unsupported');
  });

  testWidgets('installing shows the prompts a plugin brings in full', (
    tester,
  ) async {
    await tester.pumpWidget(
      _page(
        const PluginDescription(
          preview: {
            'name': 'acme-tracker',
            'trust': 'unverified',
            'servers': [
              {
                'name': 'acme',
                'url': 'https://mcp.acme.example/mcp',
                'enabled': true,
              },
            ],
            'permissions': {
              'tools': {
                'default': 'ask',
                'readOnly': 'allow',
                'tools': {'purge': 'deny'},
              },
            },
            'ui': {'mcpApps': true},
            'skills': [
              {'name': 'triage', 'body': 'Read each ticket.'},
            ],
            'commands': [
              {
                'name': 'triage',
                'prompt': 'Triage the tickets of {{project}}.',
              },
            ],
            'warnings': ['local server not run'],
          },
        ),
      ),
    );
    expect(find.text('Unverified'), findsOneWidget);
    expect(find.text('acme: https://mcp.acme.example/mcp'), findsOneWidget);
    expect(find.text('Read each ticket.'), findsOneWidget);
    expect(find.text('Triage the tickets of {{project}}.'), findsOneWidget);
    expect(find.textContaining('purge: deny'), findsOneWidget);
    expect(find.textContaining('local server not run'), findsOneWidget);
    expect(pluginToolName('acme.tracker', 'look up'), 'acme_tracker__look_up');
  });
}

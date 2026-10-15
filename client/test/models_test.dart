import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/prefs.dart';
import 'package:ryzhub/ui/model_picker.dart';

import 'kernel.dart';

Map<String, dynamic> _m(
  String id,
  String provider, {
  bool def = false,
  List<String> levels = const [],
}) => {
  'id': id,
  'provider': provider,
  'default': def,
  'reasoning_levels': levels,
};

final _catalog = {
  'chat': [
    _m('fixture-model', 'anthropic', def: true),
    _m('gpt-5.4', 'openai', levels: ['low', 'medium', 'high', 'xhigh']),
  ],
  'work': [
    {
      'harness': 'codex',
      'models': [
        _m('gpt-5.4', 'openai', def: true, levels: ['low', 'xhigh']),
      ],
    },
    {
      'harness': 'claude',
      'models': [
        _m('claude-sonnet-4-6', 'anthropic', def: true, levels: ['low', 'max']),
        _m('claude-opus-4-6', 'anthropic', levels: ['low', 'max']),
      ],
    },
  ],
};

void main() {
  setUpAll(() => prefs.language = 'zh');

  test('draft reasoning levels follow the chosen model', () {
    final hub = HubController();
    addTearDown(hub.dispose);
    hub.orgModels = _catalog;
    hub.startDraft(mode: 'chat');
    expect(hub.draftModel, 'fixture-model');
    expect(hub.draftReasoningLevels, isEmpty);
    hub.updateDraft(() => hub.draftModel = 'gpt-5.4');
    expect(hub.draftReasoningLevels.last, 'xhigh');
    hub.draftReasoning = 'high';
    // Switching to a model without reasoning control drops the choice.
    hub.updateDraft(() => hub.draftModel = 'fixture-model');
    expect(hub.draftReasoning, isNull);
    hub.startDraft(mode: 'work', harness: 'claude');
    expect(hub.draftModel, 'claude-sonnet-4-6');
    expect(hub.draftReasoningLevels.last, 'max');
  });

  test('a draft keeps to models and harnesses the server offers', () {
    final hub = HubController();
    addTearDown(hub.dispose);
    hub.orgModels = {
      'chat': _catalog['chat'],
      'work': [(_catalog['work'] as List).first],
    };
    hub.projects = [
      {
        'id': 'p',
        'state': 'active',
        'defaults': {
          'mode': 'work',
          'harness': 'claude',
          'model': 'claude-opus-4-6',
        },
      },
    ];
    hub.selected = 'x';
    hub.startDraft(project: 'p');
    expect(hub.isDraft, isTrue);
    expect(
      (hub.mode, hub.harness, hub.draftModel),
      ('work', 'codex', 'gpt-5.4'),
    );
    hub.orgModels = _catalog;
    hub.startDraft(project: 'p');
    expect((hub.harness, hub.draftModel), ('claude', 'claude-opus-4-6'));
    hub.startDraft(mode: 'chat', model: 'retired-model');
    expect(hub.draftModel, 'fixture-model');
    // Without a catalog the draft sends no model and the server decides.
    hub.orgModels = {};
    hub.startDraft(mode: 'chat');
    expect(hub.draftModel, isNull);
  });

  testWidgets('Work picks a model inside its harness submenu', (tester) async {
    final picked = <(String?, String)>[];
    await tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
        home: Scaffold(
          body: Center(
            child: ModelPicker(
              mode: 'work',
              chat: const [],
              work: [
                for (final g in _catalog['work'] as List)
                  Map<String, dynamic>.from(g),
              ],
              harness: 'codex',
              model: 'gpt-5.4',
              onPick: (h, id, _) => picked.add((h, id)),
            ),
          ),
        ),
      ),
    );
    expect(find.text('gpt-5.4'), findsOneWidget);
    await tester.tap(find.bySemanticsLabel(RegExp('模型')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Claude Code'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('claude-opus-4-6'));
    await tester.pumpAndSettle();
    expect(picked, [('claude', 'claude-opus-4-6')]);
  });

  testWidgets('another provider starts a new chat instead of switching', (
    tester,
  ) async {
    final picked = <String>[], fresh = <String>[];
    await tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
        home: Scaffold(
          body: Center(
            child: ModelPicker(
              mode: 'chat',
              chat: [
                for (final e in _catalog['chat'] as List)
                  Map<String, dynamic>.from(e),
              ],
              work: const [],
              model: 'fixture-model',
              inPlace: (_, id) => id == 'fixture-model',
              onPick: (_, id, _) => picked.add(id),
              onNewChat: (_, id) => fresh.add(id),
            ),
          ),
        ),
      ),
    );
    await tester.tap(find.bySemanticsLabel(RegExp('模型')));
    await tester.pumpAndSettle();
    expect(find.text('新对话'), findsOneWidget);
    await tester.tap(find.text('gpt-5.4'));
    await tester.pumpAndSettle();
    expect(picked, isEmpty);
    expect(fresh, ['gpt-5.4']);
  });

  testWidgets('reasoning level follows the model name and is picked there', (
    tester,
  ) async {
    final levels = <String>[];
    await tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
        home: Scaffold(
          body: Center(
            child: ModelPicker(
              mode: 'chat',
              chat: [
                for (final e in _catalog['chat'] as List)
                  Map<String, dynamic>.from(e),
              ],
              work: const [],
              model: 'gpt-5.4',
              onPick: (_, id, level) => levels.add('$id:$level'),
              levelsFor: (_, _) => const ['low', 'medium', 'high'],
            ),
          ),
        ),
      ),
    );
    // No "Think" prefix: the server default reads as its level.
    expect(find.text('中'), findsOneWidget);
    expect(find.textContaining('思考'), findsNothing);
    await tester.tap(find.bySemanticsLabel(RegExp('模型')));
    await tester.pumpAndSettle();
    // Levels open in a flyout beside the model; a touch tap opens it.
    await tester.tap(find.text('gpt-5.4').last);
    await tester.pumpAndSettle();
    await tester.tap(find.text('高'));
    await tester.pumpAndSettle();
    expect(levels, ['gpt-5.4:high']);
  });
}

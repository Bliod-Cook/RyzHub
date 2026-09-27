import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/l10n.dart';
import 'package:ryzhub/main.dart';
import 'package:ryzhub/ui/dialogs.dart';
import 'package:ryzhub/ui/model_picker.dart';
import 'package:ryzhub/ui/sidebar.dart';

void main() {
  tearDown(() => prefs.language = 'zh');

  test('language follows the first Chinese or English device locale', () {
    expect(detectLanguage(const [Locale('zh', 'TW')]), 'zh');
    expect(detectLanguage(const [Locale('fr', 'FR')]), 'en');
    expect(detectLanguage(const []), 'en');
    expect(detectLanguage(const [Locale('ja'), Locale('zh', 'CN')]), 'zh');
    expect(detectLanguage(const [Locale('en', 'US'), Locale('zh')]), 'en');
  });

  testWidgets('composer speaks English when the client language is English', (
    tester,
  ) async {
    prefs.language = 'en';
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: Composer(onSend: (_) async {}, onCancel: () {}, running: false),
        ),
      ),
    );
    expect(find.byTooltip('Send'), findsOneWidget);
    expect(find.text('Ask anything…'), findsOneWidget);
  });

  testWidgets('composer stays on one row until the text reaches the controls', (
    tester,
  ) async {
    await tester.pumpWidget(
      MaterialApp(
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
    final field = find.byType(TextField), send = find.byTooltip('发送');
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

  test('titles come from the first readable line of the first message', () {
    prefs.language = 'zh';
    expect(conversationTitle('\n```dart\nmain();\n```'), 'main();');
    expect(conversationTitle('  你好   世界  \n第二行'), '你好 世界');
    expect(conversationTitle('字' * 41), '${'字' * 40}…');
    expect(conversationTitle('   \n'), '新对话');
  });

  test('sidebar sections split pinned, project, archived and recent chats', () {
    final hub = HubController();
    addTearDown(hub.dispose);
    hub.conversations.addAll([
      {'id': 'a', 'state': 'active', 'project_id': null},
      {'id': 'b', 'state': 'active', 'project_id': 'p'},
      {'id': 'c', 'state': 'archived', 'project_id': null},
      {'id': 'd', 'state': 'active', 'project_id': null, 'pinned': true},
    ]);
    expect(hub.recents.map((c) => c['id']), ['a']);
    expect(hub.projectConversations('p').map((c) => c['id']), ['b']);
    expect(hub.archived.map((c) => c['id']), ['c']);
    expect(hub.pinned.map((c) => c['id']), ['d']);
  });

  Map<String, dynamic> m(
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
  final catalog = {
    'chat': [
      m('fixture-model', 'anthropic', def: true),
      m('gpt-5.4', 'openai', levels: ['low', 'medium', 'high', 'xhigh']),
    ],
    'work': [
      {
        'harness': 'codex',
        'models': [
          m('gpt-5.4', 'openai', def: true, levels: ['low', 'xhigh']),
        ],
      },
      {
        'harness': 'claude',
        'models': [
          m(
            'claude-sonnet-4-6',
            'anthropic',
            def: true,
            levels: ['low', 'max'],
          ),
          m('claude-opus-4-6', 'anthropic', levels: ['low', 'max']),
        ],
      },
    ],
  };

  test('draft reasoning levels follow the chosen model', () {
    final hub = HubController();
    addTearDown(hub.dispose);
    hub.orgModels = catalog;
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
      'chat': catalog['chat'],
      'work': [(catalog['work'] as List).first],
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
      {
        'id': 'legacy',
        'state': 'active',
        'defaults': {'mode': 'chat', 'protocol': 'responses'},
      },
    ];
    hub.selected = 'x';
    hub.startDraft(project: 'p');
    expect(hub.isDraft, isTrue);
    expect(
      (hub.mode, hub.harness, hub.draftModel),
      ('work', 'codex', 'gpt-5.4'),
    );
    hub.orgModels = catalog;
    hub.startDraft(project: 'p');
    expect((hub.harness, hub.draftModel), ('claude', 'claude-opus-4-6'));
    // Older projects stored a wire protocol; the draft shows that provider's model.
    hub.startDraft(project: 'legacy');
    expect((hub.mode, hub.draftModel), ('chat', 'gpt-5.4'));
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
        home: Scaffold(
          body: Center(
            child: ModelPicker(
              mode: 'work',
              chat: const [],
              work: [
                for (final g in catalog['work'] as List)
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
        home: Scaffold(
          body: Center(
            child: ModelPicker(
              mode: 'chat',
              chat: [
                for (final e in catalog['chat'] as List)
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
        home: Scaffold(
          body: Center(
            child: ModelPicker(
              mode: 'chat',
              chat: [
                for (final e in catalog['chat'] as List)
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

  testWidgets('connection status reconnects only when unhealthy', (
    tester,
  ) async {
    var reconnects = 0;
    Widget status(String connection) => MaterialApp(
      home: Scaffold(
        body: ConnectionStatus(
          connection: connection,
          onReconnect: () => reconnects++,
        ),
      ),
    );
    await tester.pumpWidget(status('online'));
    expect(find.bySemanticsLabel('重新连接'), findsNothing);
    await tester.tap(find.byType(ConnectionStatus));
    expect(reconnects, 0);
    await tester.pumpWidget(status('retrying'));
    // Only a dot: the state is in its colour and tooltip, not in text.
    expect(find.text('重连中'), findsNothing);
    expect(find.bySemanticsLabel('重新连接'), findsOneWidget);
    await tester.tap(find.byType(ConnectionStatus));
    expect(reconnects, 1);
  });

  test('pin and delete stay hidden until the server advertises them', () {
    final hub = HubController();
    addTearDown(hub.dispose);
    final conv = {'id': 'a', 'state': 'active', 'title': 't'};
    List<Object?> values() => conversationMenu(
      hub,
      conv,
      header: false,
    ).whereType<PopupMenuItem<String>>().map((e) => e.value).toList();
    bool enabled(String value) => conversationMenu(hub, conv, header: false)
        .whereType<PopupMenuItem<String>>()
        .firstWhere((e) => e.value == value)
        .enabled;
    // Pin is reserved: listed but disabled; delete stays hidden.
    expect(enabled('pin'), isFalse);
    expect(values(), isNot(contains('delete')));
    expect(() => hub.pin('a', true), throwsException);
    hub.capabilities = {'conversation_pin': true, 'conversation_delete': true};
    expect(values(), containsAll(['pin', 'delete']));
    expect(enabled('pin'), isTrue);
  });
}

import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/prefs.dart';
import 'package:ryzhub/rust/api.dart';
import 'package:ryzhub/ui/dialogs.dart';
import 'package:ryzhub/ui/sidebar.dart';

import 'kernel.dart';

void main() {
  setUpAll(() => prefs.language = 'zh');

  test('titles come from the first readable line of the first message', () {
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
      {'id': 'd', 'state': 'active', 'project_id': null, 'pinned_at_ms': 5},
      {'id': 'e', 'state': 'active', 'project_id': null, 'pinned_at_ms': 9},
      {'id': 'f', 'state': 'archived', 'project_id': null, 'pinned_at_ms': 7},
    ]);
    expect(hub.recents.map((c) => c['id']), ['a']);
    expect(hub.projectConversations('p').map((c) => c['id']), ['b']);
    expect(hub.archived.map((c) => c['id']), ['c', 'f']);
    // Most recently pinned first; an archived pin waits until it returns.
    expect(hub.pinned.map((c) => c['id']), ['e', 'd']);
  });

  test('a pin from another device moves the chat as it arrives', () async {
    final hub = HubController()
      ..org = 'o'
      ..user = 'u';
    addTearDown(hub.dispose);
    hub.conversations.addAll([
      {'id': 'a', 'title': 'A', 'state': 'active', 'project_id': null},
      {'id': 'b', 'title': 'B', 'state': 'active', 'project_id': null},
    ]);
    // The user's channel as the kernel hands it over.
    final core = ClientCore(records: []);
    core.subscribe(channel: 'user:o:u');
    Future<void> pin(String id, int revision, int pinnedAt) => hub.debugChanges(
      core
          .mergeHistory(
            channel: 'user:o:u',
            itemsJson: jsonEncode([
              {
                'id': id,
                'revision': revision,
                'kind': 'conversation',
                'state': 'active',
                'text': id.toUpperCase(),
                'detail': {
                  'kind': {
                    'Conversation': {'pinned_at_ms': pinnedAt},
                  },
                },
              },
            ]),
          )
          .changesJson,
    );
    await pin('b', 1, 42);
    expect(hub.pinned.map((c) => c['id']), ['b']);
    expect(hub.recents.map((c) => c['id']), ['a']);
    await pin('b', 2, 0);
    expect(hub.pinned, isEmpty);
    expect(hub.recents.map((c) => c['id']), ['a', 'b']);
  });

  testWidgets('connection status reconnects only when unhealthy', (
    tester,
  ) async {
    var reconnects = 0;
    Widget status(String connection) => MaterialApp(
      theme: hubTheme,
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
    // Pin is a button beside the menu, never an entry; delete stays hidden.
    expect(values(), isNot(contains('pin')));
    expect(values(), isNot(contains('delete')));
    hub.capabilities = {'conversation_delete': true};
    expect(values(), contains('delete'));
    expect(values(), isNot(contains('pin')));
  });
}

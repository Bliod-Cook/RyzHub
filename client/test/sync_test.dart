import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/rust/api.dart';
import 'package:ryzhub/storage/cache.dart';

class _Cache implements LocalCache {
  final records = <String, String?>{};
  @override
  Future<void> write(Map<String, String?> values) async =>
      records.addAll(values);
  @override
  Future<void> close() async {}
  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

HubController _hub(ClientCore core, _Cache cache) =>
    HubController.testing(
        core: core,
        cache: cache,
        connect: (_) => throw UnimplementedError(),
      )
      ..org = 'o'
      ..user = 'u'
      ..token = 't';

void main() {
  test(
    'a new chat is created by command, shown at once, withdrawn when refused',
    () async {
      final core = ClientCore(records: []);
      final cache = _Cache();
      final hub = _hub(core, cache);
      addTearDown(hub.dispose);
      await hub.send('第一句话');
      final created = hub.conversations.single;
      expect(created['creating'], isTrue);
      expect(created['title'], '第一句话');
      expect(hub.selected, created['id']);
      // The command waits in the stored queue; the conversation's own frames wait for it.
      final pending =
          jsonDecode(cache.records['engine']!)['pending_commands'] as Map;
      final cid = pending.keys.single as String;
      expect(pending[cid]['channel'], 'user:o:u');
      expect(
        core.retryPending(),
        hasLength(1),
        reason: 'only the CreateRun goes out',
      );
      hub.debugResults(
        jsonEncode([
          {
            'cid': cid,
            'channel': 'user:o:u',
            'accepted': false,
            'code': 'CONVERSATION_ID_TAKEN',
            'retryable': false,
          },
        ]),
      );
      expect(hub.conversations, isEmpty);
      expect(hub.isDraft, isTrue);
      expect(hub.failedSend?.text, '第一句话');
    },
  );

  test(
    'conversations with news since they were read are unread; drafts arrive',
    () async {
      final hub = _hub(ClientCore(records: []), _Cache());
      addTearDown(hub.dispose);
      hub.conversations.addAll([
        {'id': 'a', 'title': 'A', 'state': 'active', 'activity_seq': 0},
        {'id': 'b', 'title': 'B', 'state': 'active', 'activity_seq': 0},
      ]);
      Map<String, dynamic> conversation(String id, int activity) => {
        'id': id,
        'kind': 'conversation',
        'state': 'active',
        'text': id.toUpperCase(),
        'detail': {
          'kind': {
            'Conversation': {'pinned_at_ms': 0, 'activity_seq': activity},
          },
        },
      };
      await hub.debugChanges(
        jsonEncode([
          {
            'channel': 'user:o:u',
            'items': [conversation('a', 7), conversation('b', 5)],
            'actions': [],
            'read_states': {'a': 7, 'b': 2},
            'drafts': {
              'b': {
                'conversation_id': 'b',
                'text': '写了一半',
                'device_id': 'other',
              },
            },
            'cursor': 3,
          },
        ]),
      );
      final [a, b] = hub.conversations;
      expect(hub.unread(a), isFalse, reason: 'read up to its activity');
      expect(hub.unread(b), isTrue);
      expect(hub.drafts['b']?['text'], '写了一半');
      // The open conversation is never unread.
      hub.selected = 'b';
      expect(hub.unread(b), isFalse);
    },
  );

  test('timeline diffs keep the blocks they do not replace', () async {
    final core = ClientCore(records: []);
    final hub = HubController()..selected = 'c';
    addTearDown(hub.dispose);
    Future<void> merge(List<Map<String, dynamic>> items) => hub.debugChanges(
      core
          .mergeHistory(channel: 'conv:c', itemsJson: jsonEncode(items))
          .changesJson,
    );
    await hub.debugChanges(core.subscribe(channel: 'conv:c').changesJson);
    Map<String, dynamic> reply(int revision, String text) => {
      'id': 'a',
      'revision': revision,
      'kind': 'assistant',
      'state': 'streaming',
      'text': text,
      'first_seq': 1,
    };
    await merge([reply(1, 'one\n\ntw')]);
    final first = hub.entry('a')!.blocks.first;
    await merge([
      reply(2, 'one\n\ntwo'),
      {
        'id': 'b',
        'revision': 1,
        'kind': 'user',
        'state': 'completed',
        'text': 'hi',
        'first_seq': 2,
      },
    ]);
    final a = hub.entry('a')!;
    expect(a.item['text'], 'one\n\ntwo');
    expect(
      identical(a.blocks.first, first),
      isTrue,
      reason: 'kept, not rebuilt',
    );
    expect(a.blocks.last['inlines'][0]['text'], 'two');
    expect(hub.rows.map((r) => r['id']), ['a', 'b']);
    expect(hub.items.map((i) => i['id']), ['a', 'b']);
  });
}

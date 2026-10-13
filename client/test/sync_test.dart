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

HubController _hub(ClientCore core) =>
    HubController.testing(
        core: core,
        cache: _Cache(),
        connect: (_) => throw UnimplementedError(),
      )
      ..org = 'o'
      ..user = 'u'
      ..token = 't'
      ..capabilities = {'create_run': true, 'unread': true, 'drafts': true};

void main() {
  test(
    'a new chat is created by command, shown at once, withdrawn when refused',
    () async {
      final core = ClientCore(records: []);
      final hub = _hub(core);
      addTearDown(hub.dispose);
      await hub.send('第一句话');
      final created = hub.conversations.single;
      expect(created['creating'], isTrue);
      expect(created['title'], '第一句话');
      expect(hub.selected, created['id']);
      // The command waits in the kernel's queue; the conversation's own frames wait for it.
      final pending = jsonDecode(core.viewJson())['pending_commands'] as Map;
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
      final hub = _hub(ClientCore(records: []));
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
}

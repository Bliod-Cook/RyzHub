import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/ui/workspace_status.dart';

/// A hub whose requests wait until the test answers them.
class _Hub extends HubController {
  final answers = <String, Completer<dynamic>>{};
  _Hub() {
    org = 'org';
    capabilities = {'work': 'real'};
    conversations.addAll([
      {'id': 'a', 'mode': 'work'},
      {'id': 'b', 'mode': 'work'},
    ]);
  }

  @override
  Future<dynamic> request(
    String path, {
    Map<String, dynamic>? body,
    String? method,
    Duration timeout = const Duration(seconds: 15),
  }) => (answers[path] = Completer()).future;
}

void main() {
  test(
    'a workspace answer for a conversation left behind is dropped',
    () async {
      final hub = _Hub()..selected = 'a';
      final first = hub.refreshWorkspace();
      hub.selected = 'b';
      final second = hub.refreshWorkspace();
      hub.answers['/orgs/org/conversations/b/workspace']!.complete({'id': 'b'});
      await second;
      hub.answers['/orgs/org/conversations/a/workspace']!.complete({'id': 'a'});
      await first;
      expect(hub.workspace['id'], 'b');
    },
  );

  test('checkpoints show how long ago they were saved', () {
    final now = DateTime.utc(2026, 10, 12, 12);
    String saved(Duration ago) =>
        checkpointSaved(now.subtract(ago).toIso8601String(), now: now);
    expect(saved(const Duration(seconds: 20)), 'Checkpoint saved · just now');
    expect(saved(const Duration(minutes: 7)), 'Checkpoint saved · 7 min ago');
    expect(saved(const Duration(hours: 3)), 'Checkpoint saved · 3 h ago');
    expect(
      saved(const Duration(days: 2)),
      startsWith('Checkpoint saved · Oct 10'),
    );
    expect(checkpointSaved(null), 'Checkpoint saved');
  });
}

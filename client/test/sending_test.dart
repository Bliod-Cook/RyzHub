import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';

void main() {
  test('a refused message comes back to edit; accepted ones are forgotten', () {
    final hub = HubController();
    const files = [
      {'id': 'f1', 'name': 'notes.csv', 'size': 5, 'kind': 'text'},
    ];
    hub.debugQueued(
      'c1',
      const SentInput(channel: 'conv:a', text: 'read this', files: files),
    );
    hub.debugQueued(
      'c2',
      const SentInput(channel: 'conv:a', text: 'and this', files: []),
    );
    hub.debugQueued(
      'c3',
      const SentInput(channel: 'conv:a', text: 'busy', files: []),
    );
    hub.debugResults(
      '[{"cid":"c2","channel":"conv:a","accepted":true,"code":"","retryable":false},'
      '{"cid":"c3","channel":"conv:a","accepted":false,"code":"RATE_LIMITED","retryable":true},'
      '{"cid":"c1","channel":"conv:a","accepted":false,"code":"ATTACHMENT_UNAVAILABLE","retryable":false}]',
    );
    final failed = hub.takeFailedSend()!;
    expect(failed.text, 'read this');
    expect(failed.files.single['id'], 'f1');
    expect(failed.code, 'ATTACHMENT_UNAVAILABLE');
    expect(hub.takeFailedSend(), isNull);
    // A retryable refusal stays on its way; its later refusal still finds it.
    hub.debugResults(
      '[{"cid":"c3","channel":"conv:a","accepted":false,"code":"CONFLICT","retryable":false}]',
    );
    expect(hub.takeFailedSend()!.text, 'busy');
    // A command with no message behind it reports as before.
    hub.debugResults(
      '[{"cid":"zz","channel":"conv:a","accepted":false,"code":"FORBIDDEN","retryable":false}]',
    );
    expect(hub.error, 'FORBIDDEN');
  });

  test('a refused setting drops only that conversation\'s changes', () {
    final hub = HubController()..selected = 'a';
    hub.debugPendingConfig('conv:a', {'reasoning': 'high'});
    hub.debugPendingConfig('conv:b', {'reasoning': 'low'});
    hub.debugResults(
      '[{"cid":"x","channel":"conv:b","accepted":false,"code":"INVALID_REASONING","retryable":false}]',
    );
    expect(hub.settings['reasoning'], 'high');
    hub.selected = 'b';
    expect(hub.settings['reasoning'], isNull);
  });
}

import 'dart:async';
import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/main.dart';
import 'package:ryzhub/ui/attachments.dart';

/// A hub whose server calls are recorded and answered in place.
class _Hub extends HubController {
  final requests = <String>[];
  final uploads = <String, Completer<Map<String, dynamic>>>{};
  _Hub() {
    capabilities = {
      'attachments': {'max_bytes': 100, 'max_per_message': 2},
    };
    org = 'org';
  }

  @override
  Future<Map<String, dynamic>> uploadFile(
    String name,
    int size,
    Stream<List<int>> Function() open,
  ) => (uploads[name] = Completer()).future;

  @override
  Future<dynamic> request(
    String path, {
    Map<String, dynamic>? body,
    String? method,
  }) async {
    requests.add('${method ?? 'GET'} $path');
    return {};
  }
}

/// A PNG of [width] × [height] pixels.
Future<Uint8List> _png(int width, int height) async {
  final recorder = ui.PictureRecorder();
  Canvas(recorder).drawRect(
    Rect.fromLTWH(0, 0, width.toDouble(), height.toDouble()),
    Paint()..color = Colors.teal,
  );
  final image = await recorder.endRecording().toImage(width, height);
  final data = await image.toByteData(format: ui.ImageByteFormat.png);
  return data!.buffer.asUint8List();
}

/// [provider] as decoded, width × height.
Future<(int, int)> _decoded(ImageProvider provider) {
  final done = Completer<(int, int)>();
  provider
      .resolve(ImageConfiguration.empty)
      .addListener(
        ImageStreamListener(
          (info, _) => done.complete((info.image.width, info.image.height)),
          onError: done.completeError,
        ),
      );
  return done.future;
}

void main() {
  setUpAll(() => prefs.language = 'en');

  testWidgets('a square thumbnail does not magnify a wide picture', (
    tester,
  ) async {
    await tester.runAsync(() async {
      final wide = await _png(800, 200);
      // Its short edge covers the square; the width is cropped, not shrunk.
      expect(await _decoded(CoverImage(wide, 120)), (480, 120));
      expect(await _decoded(CoverImage(await _png(150, 600), 120)), (120, 480));
      // A picture smaller than the square stays as it is.
      expect(await _decoded(CoverImage(wide, 300)), (800, 200));
    });
  });

  test(
    'files upload for the next message and only uploads are deleted',
    () async {
      final hub = _Hub();
      final upload = hub.attach('a.png', 10, () => const Stream.empty());
      expect(hub.filesBusy, isTrue);
      expect(hub.readyFiles, isEmpty);
      hub.uploads['a.png']!.complete({'id': 'f1', 'kind': 'image'});
      await upload;
      expect(hub.readyFiles, ['f1']);
      expect(hub.pendingFiles.single.kind, 'image');
      // Too large, and over the per-message count, never leave the client.
      await hub.attach('big.bin', 101, () => const Stream.empty());
      expect(hub.pendingFiles.last.error, 'ATTACHMENT_TOO_LARGE');
      expect(hub.uploads.containsKey('big.bin'), isFalse);
      hub.detach(hub.pendingFiles.last);
      hub.attachFromLibrary([
        {'id': 'f2', 'name': 'old.pdf', 'size': 5, 'kind': 'pdf'},
        {'id': 'f2', 'name': 'old.pdf', 'size': 5, 'kind': 'pdf'},
      ]);
      expect(hub.readyFiles, ['f1', 'f2']);
      await hub.attach('third.txt', 1, () => const Stream.empty());
      expect(hub.pendingFiles.last.error, 'TOO_MANY_ATTACHMENTS');
      hub.detach(hub.pendingFiles.last);
      // Taking a library file off the message keeps it; an upload is deleted.
      hub.detach(hub.pendingFiles.firstWhere((f) => f.id == 'f2'));
      hub.detach(hub.pendingFiles.single);
      await Future<void>.delayed(Duration.zero);
      expect(hub.requests, ['DELETE /orgs/org/attachments/f1']);
    },
  );

  test('a file removed while it uploads is deleted once it arrives', () async {
    final hub = _Hub();
    final upload = hub.attach('a.png', 10, () => const Stream.empty());
    hub.detach(hub.pendingFiles.single);
    hub.uploads['a.png']!.complete({'id': 'late', 'kind': 'image'});
    await upload;
    await Future<void>.delayed(Duration.zero);
    expect(hub.requests, ['DELETE /orgs/org/attachments/late']);
    expect(hub.pendingFiles, isEmpty);
  });

  testWidgets(
    'the composer sends files without text, but not while uploading',
    (tester) async {
      final sent = <String>[];
      Future<void> pump({required bool ready, required bool blocked}) =>
          tester.pumpWidget(
            MaterialApp(
              home: Scaffold(
                body: Composer(
                  onSend: (text) async => sent.add(text),
                  onCancel: () {},
                  running: false,
                  canSendEmpty: ready,
                  blocked: blocked,
                  attachments: const Text('chips'),
                ),
              ),
            ),
          );
      await pump(ready: true, blocked: true);
      expect(find.text('chips'), findsOneWidget);
      await tester.tap(find.byTooltip('Send'));
      await tester.pump();
      expect(sent, isEmpty);
      await pump(ready: true, blocked: false);
      await tester.tap(find.byTooltip('Send'));
      await tester.pump();
      expect(sent, ['']);
      await pump(ready: false, blocked: false);
      await tester.tap(find.byTooltip('Send'));
      await tester.pump();
      expect(sent, ['']);
    },
  );

  testWidgets('a message shows the files it carried, even without text', (
    tester,
  ) async {
    final hub = _Hub();
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: MessageCard(
            item: {
              'id': 'u',
              'kind': 'user',
              'state': 'completed',
              'text': '',
              'detail': {
                'kind': {
                  'Attachments': {
                    'files': [
                      {
                        'id': 'f1',
                        'path': 'notes.csv',
                        'name': 'notes.csv',
                        'kind': 'text',
                        'size': 2048,
                      },
                    ],
                  },
                },
              },
            },
            files: (files) => MessageFiles(hub: hub, files: files),
          ),
        ),
      ),
    );
    expect(find.text('notes.csv'), findsOneWidget);
    expect(find.text('2.0 KB'), findsOneWidget);
    expect(find.byIcon(Icons.table_chart_outlined), findsOneWidget);
  });

  testWidgets('pending files show progress, size and refusals', (tester) async {
    final hub = _Hub();
    unawaited(hub.attach('a.png', 10, () => const Stream.empty()));
    await hub.attach('big.bin', 101, () => const Stream.empty());
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(body: PendingFiles(hub: hub)),
      ),
    );
    expect(find.text('Uploading…'), findsOneWidget);
    expect(find.text('The file is too large.'), findsOneWidget);
    hub.uploads['a.png']!.complete({'id': 'f1', 'kind': 'file'});
    await tester.pump();
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(body: PendingFiles(hub: hub)),
      ),
    );
    expect(find.text('10 B'), findsOneWidget);
    await tester.tap(find.byTooltip('Remove').first);
    expect(hub.pendingFiles.length, 1);
  });
}

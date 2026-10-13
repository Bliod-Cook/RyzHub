import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:crypto/crypto.dart';
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
    Stream<List<int>> Function() open, {
    String? to,
  }) => (uploads[name] = Completer()).future;

  @override
  Future<dynamic> request(
    String path, {
    Map<String, dynamic>? body,
    String? method,
    Duration timeout = const Duration(seconds: 15),
  }) async {
    requests.add('${method ?? 'GET'} $path');
    return {};
  }

  /// Library files by their content's hash.
  final contents = <String, Map<String, dynamic>>{};

  @override
  Future<Map<String, dynamic>?> fileWithHash(String sha256) async =>
      contents[sha256];

  /// [bytes] as the library's file [id], named [name].
  Future<void> holds(String id, String name, List<int> bytes) async {
    final hash = await contentHash(() => Stream.value(bytes));
    contents[hash] = {
      'id': id,
      'name': name,
      'size': bytes.length,
      'kind': 'text',
      'direction': 'upload',
      'sha256': hash,
      'created_at': '2026-09-18T10:00:00Z',
    };
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

  test(
    'content is hashed as the server names it, however it arrives',
    () async {
      const abc =
          'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad';
      expect(await contentHash(() => Stream.value(utf8.encode('abc'))), abc);
      expect(
        await contentHash(
          () => Stream.fromIterable([utf8.encode('a'), utf8.encode('bc')]),
        ),
        abc,
      );
      // A whole file in one chunk, as a browser reads it, is hashed in slices.
      final large = Uint8List.fromList(
        List.generate(3 * 1024 * 1024 + 7, (i) => i * 31 % 251),
      );
      expect(
        await contentHash(() => Stream.value(large)),
        '${sha256.convert(large)}',
      );
    },
  );

  test(
    'a file the library holds goes as that file if the member says so',
    () async {
      final hub = _Hub();
      final bytes = utf8.encode('same');
      await hub.holds('old', 'kept.txt', bytes);
      final asked = <String>[];
      Future<bool> keep(Map<String, dynamic> existing) async {
        asked.add(existing['id'] as String);
        return true;
      }

      await hub.attach('copy.txt', 4, () => Stream.value(bytes), reuse: keep);
      expect(asked, ['old']);
      expect(hub.uploads, isEmpty);
      expect(hub.readyFiles, ['old']);
      final reused = hub.pendingFiles.single;
      expect(reused.fromLibrary, isTrue);
      expect(reused.name, 'kept.txt');
      // Already on the message: it goes once.
      await hub.attach('again.txt', 4, () => Stream.value(bytes), reuse: keep);
      expect(hub.pendingFiles, [reused]);
      // Taking it off the message keeps it in the library.
      hub.detach(reused);
      await Future<void>.delayed(Duration.zero);
      expect(hub.requests, isEmpty);

      // Uploaded again when the member wants a copy.
      final upload = hub.attach(
        'copy.txt',
        4,
        () => Stream.value(bytes),
        reuse: (_) async => false,
      );
      while (!hub.uploads.containsKey('copy.txt')) {
        await Future<void>.delayed(Duration.zero);
      }
      hub.uploads['copy.txt']!.complete({'id': 'new', 'kind': 'text'});
      await upload;
      expect(hub.readyFiles, ['new']);
      expect(hub.pendingFiles.single.fromLibrary, isFalse);
    },
  );

  testWidgets('one answer can do for every duplicate of a batch', (
    tester,
  ) async {
    final hub = _Hub();
    final first = utf8.encode('first'), second = utf8.encode('second');
    await hub.holds('l1', 'first.txt', first);
    await hub.holds('l2', 'second.txt', second);
    late BuildContext context;
    await tester.pumpWidget(
      MaterialApp(
        home: Builder(
          builder: (c) {
            context = c;
            return const SizedBox();
          },
        ),
      ),
    );
    await attachLocal(context, hub, [
      (name: 'first.txt', size: 5, open: () => Stream.value(first)),
      (name: 'b.txt', size: 6, open: () => Stream.value(second)),
    ]);
    await tester.pumpAndSettle();
    expect(find.text('Already in your library'), findsOneWidget);
    expect(find.textContaining('has the same content'), findsNothing);
    await tester.tap(find.text('Do the same for other duplicates'));
    await tester.pump();
    await tester.tap(find.text('Use existing'));
    await tester.pumpAndSettle();
    expect(find.text('Already in your library'), findsNothing);
    expect(hub.readyFiles, ['l1', 'l2']);
    expect(hub.uploads, isEmpty);
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

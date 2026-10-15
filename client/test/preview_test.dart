import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/main.dart';
import 'package:ryzhub/ui/attachments.dart';
import 'package:ryzhub/ui/preview.dart';

Map<String, dynamic> _file(
  String id,
  String name,
  String kind, {
  int size = 2048,
  String? type,
}) => {'id': id, 'name': name, 'kind': kind, 'size': size, 'media_type': type};

/// A 1×1 PNG, so pictures decode.
Uint8List _png() => base64Decode(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
);

/// A hub that serves files from memory and records what was fetched.
class _Hub extends HubController {
  final contents = <String, Uint8List>{};
  final renditions = <String, Uint8List>{};

  /// Contents that arrive only when completed.
  final pending = <String, Completer<Uint8List>>{};
  final fetched = <String>[];
  final deleted = <String>{};
  _Hub() {
    org = 'org';
  }

  @override
  Future<Uint8List> preview(String id) async {
    fetched.add('preview:$id');
    final rendition = renditions[id];
    if (rendition == null) throw Exception('ATTACHMENT_UNAVAILABLE');
    return rendition;
  }

  @override
  Future<Uint8List> fileContent(String id) async {
    fetched.add(id);
    if (deleted.contains(id)) throw Exception('ATTACHMENT_DELETED');
    return pending[id]?.future ?? contents[id]!;
  }
}

/// The bytes the preview's picture shows.
Uint8List _shown(WidgetTester tester) {
  var image = tester.widget<Image>(find.byType(Image)).image;
  if (image is ResizeImage) image = image.imageProvider;
  return (image as MemoryImage).bytes;
}

Future<void> _show(
  WidgetTester tester,
  _Hub hub, {
  Future<void> Function(Map<String, dynamic>, String)? onAsk,
  bool floating = false,
}) async {
  tester.view.physicalSize = const Size(1200, 900);
  tester.view.devicePixelRatio = 1;
  addTearDown(tester.view.reset);
  await tester.pumpWidget(
    MaterialApp(
      home: Scaffold(
        body: ListenableBuilder(
          listenable: hub,
          builder: (context, _) => hub.previewFiles == null
              ? const Text('closed')
              : FilePreviewView(
                  hub: hub,
                  onAsk: onAsk ?? (_, _) async {},
                  floating: floating,
                ),
        ),
      ),
    ),
  );
  await tester.pumpAndSettle();
}

void main() {
  setUpAll(() => prefs.language = 'en');

  test('each file previews as what it is', () {
    Preview? which(Map<String, dynamic> file) =>
        previewOf(PreviewFile.of(file));
    expect(which(_file('a', 'shot.png', 'image')), Preview.image);
    expect(which(_file('b', 'brief.pdf', 'pdf')), Preview.pdf);
    expect(which(_file('c', 'notes.csv', 'text')), Preview.text);
    // Text the server did not inline still previews, up to a size.
    expect(which(_file('d', 'main.py', 'file')), Preview.text);
    expect(
      which(_file('e', 'data', 'file', type: 'application/json')),
      Preview.text,
    );
    // A picture the server could not render when it was uploaded.
    expect(
      which(_file('p', 'shot.png', 'file', type: 'image/png')),
      Preview.image,
    );
    expect(which(_file('h', 'shot.heic', 'file', type: 'image/heic')), isNull);
    expect(which(_file('f', 'big.log', 'file', size: 5 << 20)), isNull);
    expect(which(_file('g', 'Managed.zip', 'file')), isNull);
  });

  testWidgets('a file nothing can show offers a download, unfetched', (
    tester,
  ) async {
    final hub = _Hub()..openPreview([_file('z', 'Managed.zip', 'file')]);
    await _show(tester, hub);
    expect(find.text('Managed.zip'), findsNWidgets(2));
    // The name's pill hugs the name rather than spanning the bar.
    final pill = find
        .ancestor(
          of: find.text('Managed.zip').first,
          matching: find.byType(Container),
        )
        .first;
    expect(tester.getSize(pill).width, lessThan(300));
    expect(find.text('Preview not available'), findsOneWidget);
    expect(find.text('Download file'), findsOneWidget);
    expect(find.byTooltip('Download'), findsOneWidget);
    expect(hub.fetched, isEmpty);
  });

  testWidgets('text shows as written, Markdown rendered, other bytes refused', (
    tester,
  ) async {
    final hub = _Hub()
      ..contents.addAll({
        'py': utf8.encode('def hello():\n    return 1\n'),
        'md': utf8.encode('# Title\n\nSome *words*.'),
        'bin': Uint8List.fromList([0xff, 0xfe, 0x00]),
      })
      ..openPreview([
        _file('py', 'main.py', 'file'),
        _file('md', 'README.md', 'text', type: 'text/markdown'),
        _file('bin', 'odd.txt', 'file'),
      ]);
    await _show(tester, hub);
    expect(find.textContaining('def hello()'), findsOneWidget);

    await tester.sendKeyEvent(LogicalKeyboardKey.arrowRight);
    await tester.pumpAndSettle();
    expect(hub.previewIndex, 1);
    expect(find.textContaining('# Title'), findsNothing);
    expect(find.text('Title'), findsOneWidget);

    await tester.tap(find.byTooltip('Next'));
    await tester.pumpAndSettle();
    expect(find.text('Preview not available: not a text file'), findsOneWidget);
    expect(find.byTooltip('Next'), findsNothing);

    await tester.sendKeyEvent(LogicalKeyboardKey.arrowLeft);
    await tester.pumpAndSettle();
    expect(hub.previewIndex, 1);
    await tester.sendKeyEvent(LogicalKeyboardKey.escape);
    await tester.pumpAndSettle();
    expect(find.text('closed'), findsOneWidget);
  });

  testWidgets('a picture shows its rendition, then the file as uploaded', (
    tester,
  ) async {
    final rendition = _png(), original = _png();
    final hub = _Hub()
      ..renditions['p'] = rendition
      ..pending['p'] = Completer()
      ..openPreview([_file('p', 'shot.png', 'image', type: 'image/png')]);
    await _show(tester, hub);
    expect(identical(_shown(tester), rendition), isTrue);
    hub.pending['p']!.complete(original);
    // One frame for the bytes to arrive, one to show them.
    await tester.pump();
    await tester.pump();
    expect(identical(_shown(tester), original), isTrue);
    // Stepping back to it shows it again without fetching it again.
    hub.closePreview();
    hub.openPreview([_file('p', 'shot.png', 'image', type: 'image/png')]);
    await tester.pump();
    await tester.pump();
    expect(identical(_shown(tester), original), isTrue);
    expect(hub.fetched.where((f) => f == 'p'), hasLength(1));
  });

  testWidgets('a large original is left for the download', (tester) async {
    final rendition = _png();
    final hub = _Hub()
      ..renditions['p'] = rendition
      ..openPreview([
        _file('p', 'huge.png', 'image', type: 'image/png', size: 30 << 20),
      ]);
    await _show(tester, hub);
    expect(hub.fetched, ['preview:p']);
    expect(identical(_shown(tester), rendition), isTrue);
  });

  testWidgets('a picture the server could not render shows as uploaded', (
    tester,
  ) async {
    final original = _png();
    final hub = _Hub()
      ..contents['p'] = original
      ..openPreview([_file('p', 'shot.png', 'file', type: 'image/png')]);
    await _show(tester, hub);
    expect(identical(_shown(tester), original), isTrue);
  });

  testWidgets('with neither a rendition nor a small original, no preview', (
    tester,
  ) async {
    final hub = _Hub()
      ..openPreview([
        _file('p', 'huge.png', 'file', type: 'image/png', size: 30 << 20),
      ]);
    await _show(tester, hub);
    expect(find.text('Preview not available'), findsOneWidget);
    expect(find.text('Download file'), findsOneWidget);
  });

  testWidgets('a deleted file says so and has nothing to download', (
    tester,
  ) async {
    final hub = _Hub()
      ..deleted.add('gone')
      ..openPreview([_file('gone', 'old.md', 'text')]);
    await _show(tester, hub);
    expect(find.text('This file was deleted'), findsOneWidget);
    expect(find.text('Download file'), findsNothing);
  });

  testWidgets('a question about the file goes with that file', (tester) async {
    final asked = <(String, String)>[];
    final hub = _Hub()..openPreview([_file('z', 'Managed.zip', 'file')]);
    await _show(
      tester,
      hub,
      onAsk: (file, question) async => asked.add((file['id'], question)),
    );
    expect(find.text('Ask about this file'), findsOneWidget);
    await tester.enterText(find.byType(TextField), 'What is inside?');
    await tester.tap(find.byTooltip('Send'));
    await tester.pumpAndSettle();
    expect(asked, [('z', 'What is inside?')]);
  });

  testWidgets('over the conversation, a tap beside the file closes it', (
    tester,
  ) async {
    final hub = _Hub()
      ..contents['n'] = utf8.encode('line one')
      ..openPreview([_file('n', 'notes.txt', 'text')], inConversation: true);
    final asked = <String>[];
    await _show(
      tester,
      hub,
      floating: true,
      onAsk: (_, question) async => asked.add(question),
    );
    // The page and the controls over it keep it open.
    await tester.tap(find.text('line one'));
    await tester.tap(find.text('notes.txt'));
    await tester.pumpAndSettle();
    expect(hub.previewFiles, isNotNull);
    // Questions go on in the conversation.
    expect(find.text('Follow up about this file'), findsOneWidget);
    await tester.enterText(find.byType(TextField), 'Summarize it');
    await tester.tap(find.byTooltip('Send'));
    await tester.pumpAndSettle();
    expect(asked, ['Summarize it']);
    // Beside the page, below the controls, is the blurred conversation.
    await tester.tapAt(const Offset(40, 400));
    await tester.pumpAndSettle();
    expect(find.text('closed'), findsOneWidget);
  });

  testWidgets('over the conversation, the notice of a file keeps it open', (
    tester,
  ) async {
    final hub = _Hub()
      ..openPreview([_file('z', 'Managed.zip', 'file')], inConversation: true);
    await _show(tester, hub, floating: true);
    await tester.tap(find.text('Preview not available'));
    await tester.pumpAndSettle();
    expect(hub.previewFiles, isNotNull);
    await tester.tapAt(const Offset(1100, 100));
    await tester.pumpAndSettle();
    expect(hub.previewFiles, isNull);
  });

  testWidgets('the library\'s preview stays open around the file', (
    tester,
  ) async {
    final hub = _Hub()..openPreview([_file('z', 'Managed.zip', 'file')]);
    expect(hub.previewInConversation, isFalse);
    await _show(tester, hub);
    await tester.tapAt(const Offset(1100, 100));
    await tester.tapAt(const Offset(40, 400));
    await tester.pumpAndSettle();
    expect(hub.previewFiles, isNotNull);
  });

  testWidgets('any file of a message opens the preview, not a download', (
    tester,
  ) async {
    final hub = _Hub();
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: MessageFiles(
            hub: hub,
            files: [
              _file('a', 'notes.csv', 'text'),
              _file('z', 'Managed.zip', 'file'),
            ],
          ),
        ),
      ),
    );
    await tester.tap(find.text('Managed.zip'));
    await tester.pump();
    expect(hub.previewFiles?.length, 2);
    expect(hub.previewIndex, 1);
    expect(hub.previewInConversation, isTrue);
    expect(hub.fetched, isEmpty);
  });

  test('opening a conversation or a new chat closes the preview', () {
    final hub = _Hub()..openPreview([_file('z', 'Managed.zip', 'file')]);
    hub.startDraft();
    expect(hub.previewFiles, isNull);
    hub.openPreview([_file('z', 'Managed.zip', 'file')]);
    hub.openLibrary();
    expect(hub.previewFiles, isNull);
  });
}

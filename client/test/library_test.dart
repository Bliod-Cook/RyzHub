import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/main.dart';
import 'package:ryzhub/ui/attachments.dart';
import 'package:ryzhub/ui/library.dart';

Map<String, dynamic> _file(String id, String name, String kind) => {
  'id': id,
  'name': name,
  'kind': kind,
  // A model made the report.
  'direction': id == 'f3' ? 'output' : 'upload',
  'size': 2048,
  'created_at': '2026-09-18T10:00:00Z',
  'conversations': id == 'f1' ? 2 : 0,
};

/// A hub whose library is a fixed list, filtered the way the server filters.
class _Hub extends HubController {
  final searches = <(String?, List<String>?)>[];
  final deleted = <String>[];
  final files = [
    _file('f3', 'Report.pdf', 'pdf'),
    _file('f2', 'notes.md', 'text'),
    _file('f1', 'chart.png', 'image'),
  ];
  _Hub() {
    capabilities = {
      'attachments': {'max_bytes': 100, 'max_per_message': 2},
    };
    org = 'org';
  }

  @override
  Future<List<Map<String, dynamic>>> library({
    String? before,
    String? query,
    List<String>? kinds,
    String? direction,
  }) async {
    searches.add((query, kinds));
    final words = (query ?? '').toLowerCase();
    return [
      for (final file in files)
        if ((kinds == null || kinds.contains(file['kind'])) &&
            (direction == null || file['direction'] == direction) &&
            '${file['name']}'.toLowerCase().contains(words))
          file,
    ];
  }

  @override
  Future<Uint8List> preview(String id) async => Uint8List(0);

  @override
  Future<void> deleteFile(String id) async => deleted.add(id);

  /// Library files by their content's hash.
  final contents = <String, Map<String, dynamic>>{};
  final uploaded = <String>[];

  @override
  Future<Map<String, dynamic>?> fileWithHash(String sha256) async =>
      contents[sha256];

  @override
  Future<Map<String, dynamic>> uploadFile(
    String name,
    int size,
    Stream<List<int>> Function() open, {
    String? to,
  }) async {
    uploaded.add(name);
    return _file('f${files.length + 1}', name, 'image');
  }
}

Future<void> _show(
  WidgetTester tester,
  _Hub hub, {
  Size? size,
  ValueChanged<Map<String, dynamic>>? onUse,
}) async {
  tester.view.physicalSize = size ?? const Size(1200, 900);
  tester.view.devicePixelRatio = 1;
  addTearDown(tester.view.reset);
  await tester.pumpWidget(
    MaterialApp(
      home: Scaffold(
        body: LibraryView(hub: hub, onUse: onUse ?? (_) {}),
      ),
    ),
  );
  await tester.pumpAndSettle();
}

void main() {
  setUpAll(() => prefs.language = 'en');
  setUp(() => prefs.libraryList = false);

  test('an upload date names the year only when it is not this one', () {
    final now = DateTime(2026, 10, 6);
    expect(uploadedOn('2026-09-18T10:00:00', now: now), 'Uploaded Sep 18');
    expect(uploadedOn('2025-01-02T10:00:00', now: now), 'Uploaded Jan 2, 2025');
    expect(uploadedOn(null, now: now), '');
  });

  test(
    'a file time is to the minute, with the year only when it is not this one',
    () {
      final now = DateTime(2026, 10, 6);
      expect(fileTime('2026-09-13T21:09:00', now: now), 'Sep 13, 9:09 PM');
      expect(fileTime('2026-09-13T00:05:00', now: now), 'Sep 13, 12:05 AM');
      expect(
        fileTime('2025-01-02T12:30:00', now: now),
        'Jan 2, 2025, 12:30 PM',
      );
      expect(fileTime(null, now: now), '');
    },
  );

  testWidgets('library files are picked by tapping them, up to the limit', (
    tester,
  ) async {
    final hub = _Hub();
    tester.view.physicalSize = const Size(1200, 900);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.reset);
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: Builder(
            builder: (context) => TextButton(
              onPressed: () => chooseFromLibrary(context, hub),
              child: const Text('open'),
            ),
          ),
        ),
      ),
    );
    await tester.tap(find.text('open'));
    await tester.pumpAndSettle();
    expect(find.text('Add from library'), findsOneWidget);
    expect(find.byType(Checkbox), findsNothing);
    // The actions wait for a pick.
    expect(find.text('Add to chat'), findsNothing);
    expect(find.textContaining('2.0 KB'), findsNWidgets(3));

    await tester.tap(find.text('notes.md'));
    await tester.pumpAndSettle();
    expect(find.text('1 selected'), findsOneWidget);
    expect(find.byIcon(Icons.check), findsOneWidget);
    expect(find.text('Add to chat'), findsOneWidget);

    await tester.tap(find.text('chart.png'));
    await tester.pumpAndSettle();
    expect(find.text('2 selected · limit reached'), findsOneWidget);
    // The message holds two files, so a third cannot be picked.
    await tester.tap(find.text('Report.pdf'));
    await tester.pumpAndSettle();
    expect(find.byIcon(Icons.check), findsNWidgets(2));

    await tester.tap(find.text('notes.md'));
    await tester.pumpAndSettle();
    expect(find.text('1 selected'), findsOneWidget);
    await tester.tap(find.text('Report.pdf'));
    await tester.pumpAndSettle();

    await tester.tap(find.text('Add to chat'));
    await tester.pumpAndSettle();
    expect(find.text('Add from library'), findsNothing);
    expect(hub.pendingFiles.map((f) => f.id), ['f1', 'f3']);
    expect(hub.pendingFiles.every((f) => f.fromLibrary), isTrue);
  });

  test('the library replaces the conversation until one opens', () {
    final hub = _Hub()..selected = 'c1';
    hub.openLibrary();
    expect(hub.libraryOpen, isTrue);
    expect(hub.isDraft, isTrue);
    hub.startDraft();
    expect(hub.libraryOpen, isFalse);
  });

  testWidgets('files are searched by name and filtered by kind', (
    tester,
  ) async {
    final hub = _Hub();
    await _show(tester, hub);
    expect(find.text('Report.pdf'), findsOneWidget);
    expect(find.text('notes.md'), findsOneWidget);
    // A picture fills its card; its name is for assistive tech and hover.
    expect(find.bySemanticsLabel('chart.png'), findsWidgets);
    expect(find.text('Uploaded Sep 18'), findsOneWidget);
    expect(find.text('Made by AI Sep 18'), findsOneWidget);

    await tester.enterText(find.byType(TextField), 'NOTES');
    await tester.pump(const Duration(milliseconds: 350));
    await tester.pumpAndSettle();
    expect(hub.searches.last, ('NOTES', null));
    expect(find.text('notes.md'), findsOneWidget);
    expect(find.text('Report.pdf'), findsNothing);

    await tester.tap(find.byTooltip('Clear'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Documents'));
    await tester.pumpAndSettle();
    expect(hub.searches.last.$1, '');
    expect(hub.searches.last.$2, ['pdf', 'text']);
    expect(find.bySemanticsLabel('chart.png'), findsNothing);

    await tester.tap(find.text('Other'));
    await tester.pumpAndSettle();
    expect(find.text('No matching files'), findsOneWidget);

    await tester.tap(find.text('AI-made'));
    await tester.pumpAndSettle();
    expect(find.text('Report.pdf'), findsOneWidget);
    expect(find.text('notes.md'), findsNothing);
  });

  testWidgets(
    'an upload the library holds is kept unless the member says not',
    (tester) async {
      final hub = _Hub();
      final bytes = utf8.encode('chart');
      hub.contents[await contentHash(() => Stream.value(bytes))] =
          hub.files.last;
      await _show(tester, hub);
      final library = tester.state<LibraryViewState>(find.byType(LibraryView));
      final copy = (
        name: 'chart copy.png',
        size: bytes.length,
        open: () => Stream.value(bytes),
      );
      unawaited(library.upload([copy]));
      // The upload button spins meanwhile, so nothing settles.
      await tester.pump();
      await tester.pump(const Duration(seconds: 1));
      expect(find.text('Already in your library'), findsOneWidget);
      expect(
        find.text('"chart copy.png" has the same content.'),
        findsOneWidget,
      );
      // One file: no answer for the rest.
      expect(find.byType(CheckboxListTile), findsNothing);
      await tester.tap(find.text('Keep existing'));
      await tester.pumpAndSettle();
      expect(hub.uploaded, isEmpty);
      expect(find.text('chart copy.png'), findsNothing);

      unawaited(library.upload([copy]));
      await tester.pump();
      await tester.pump(const Duration(seconds: 1));
      await tester.tap(find.text('Upload anyway'));
      await tester.pumpAndSettle();
      expect(hub.uploaded, ['chart copy.png']);
      expect(find.text('chart copy.png'), findsOneWidget);
    },
  );

  testWidgets('rows show size and use, and a file is deleted after asking', (
    tester,
  ) async {
    final hub = _Hub();
    await _show(tester, hub);
    await tester.tap(find.byTooltip('List view'));
    await tester.pumpAndSettle();
    expect(prefs.libraryList, isTrue);
    expect(
      find.text('2.0 KB · Uploaded Sep 18 · In 2 conversations'),
      findsOneWidget,
    );

    await tester.tap(find.byTooltip('File options').last);
    await tester.pumpAndSettle();
    await tester.tap(find.text('Delete'));
    await tester.pumpAndSettle();
    expect(find.textContaining('removed from the 2 conversations'), findsOne);
    await tester.tap(find.widgetWithText(FilledButton, 'Delete'));
    await tester.pumpAndSettle();
    expect(hub.deleted, ['f1']);
    expect(find.text('chart.png'), findsNothing);
    expect(find.text('Report.pdf'), findsOneWidget);
  });

  testWidgets('a picture\'s menu still acts once the pointer leaves for it', (
    tester,
  ) async {
    final used = <String>[];
    await _show(tester, _Hub(), onUse: (file) => used.add(file['id']));
    final mouse = await tester.createGesture(kind: PointerDeviceKind.mouse);
    await mouse.addPointer(
      location: tester.getCenter(find.bySemanticsLabel('chart.png').last),
    );
    addTearDown(mouse.removePointer);
    await tester.pumpAndSettle();
    await tester.tap(find.byTooltip('File options').last);
    await tester.pumpAndSettle();
    // Over the menu, the card is no longer hovered and its name fades out.
    await mouse.moveTo(tester.getCenter(find.text('Use in new chat')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Use in new chat'));
    await tester.pumpAndSettle();
    expect(used, ['f1']);
  }, variant: TargetPlatformVariant.only(TargetPlatform.linux));

  testWidgets('a narrow screen stacks the search under the title', (
    tester,
  ) async {
    final hub = _Hub()..files.clear();
    await _show(tester, hub, size: const Size(390, 800));
    expect(find.text('No files yet'), findsOneWidget);
    final title = tester.getRect(find.text('Library'));
    final search = tester.getRect(find.byType(TextField));
    expect(search.top, greaterThan(title.bottom));
    expect(tester.takeException(), isNull);
  });
}

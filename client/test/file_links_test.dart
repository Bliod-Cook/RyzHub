import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/main.dart';
import 'package:ryzhub/ui/messages.dart';

Map<String, dynamic> _reply(List<Map<String, dynamic>> files, {String? id}) => {
  'id': id ?? 'r',
  'kind': 'assistant',
  'state': 'completed',
  'text': '',
  'detail': {
    'kind': {
      'Answer': {'citations': [], 'files': files},
    },
  },
};

Map<String, dynamic> _shared(String link, {String? id, String? error}) => {
  'link': link,
  'file': id == null
      ? null
      : {'id': id, 'name': link.split('/').last, 'kind': 'text', 'size': 3},
  'error': error ?? '',
};

void main() {
  setUpAll(() => prefs.language = 'en');

  test('file links name sandbox paths, not pages', () {
    expect(isFileLink('sandbox:/workspace/out/report.pdf'), isTrue);
    expect(isFileLink('/workspace/a.txt'), isTrue);
    expect(isFileLink('sandbox:/attachments/notes.csv'), isTrue);
    expect(isFileLink('https://example.com/workspace/a.txt'), isFalse);
    expect(isFileLink('/orgs/o/conversations/c'), isFalse);
    expect(
      fileLinkPath('sandbox:/workspace/my%20file.txt'),
      '/workspace/my file.txt',
    );
    expect(fileLinkPath('/workspace/100%'), '/workspace/100%');
  });

  test('a link opens what its reply handed over, or says why not', () {
    final reply = _reply([
      _shared('/workspace/a.txt', id: 'f1'),
      _shared('/workspace/gone.txt', error: 'FILE_NOT_FOUND'),
      _shared('/workspace/b.txt', id: 'f2'),
    ]);
    final found = linkedFile('sandbox:/workspace/b.txt', [reply])!;
    expect(found.error, isNull);
    expect([for (final f in found.files) f['id']], ['f1', 'f2']);
    expect(found.index, 1);
    expect(linkedFile('/workspace/gone.txt', [reply])!.error, 'FILE_NOT_FOUND');
    expect(linkedFile('/workspace/never.txt', [reply]), isNull);
    // The reply holding the link comes first: an older version stays its own.
    final newer = _reply([_shared('/workspace/a.txt', id: 'f9')], id: 'n');
    expect(
      linkedFile('/workspace/a.txt', [reply, newer])!.files.first['id'],
      'f1',
    );
    expect(
      linkedFile('/workspace/a.txt', [newer, reply])!.files.first['id'],
      'f9',
    );
  });

  testWidgets('a reply shows file links with a glyph and hands them over', (
    tester,
  ) async {
    final tapped = <String>[];
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: MessageCard(
            item: {
              ..._reply([_shared('/workspace/out/report.txt', id: 'f1')]),
              'text':
                  'Here is [the report](sandbox:/workspace/out/report.txt) '
                  'and [a page](https://example.com).',
            },
            onSource: tapped.add,
            files: (_) => const SizedBox(),
          ),
        ),
      ),
    );
    expect(find.byIcon(Icons.insert_drive_file_outlined), findsOneWidget);
    await tester.tap(find.text('the report'));
    expect(tapped, ['sandbox:/workspace/out/report.txt']);
  });
}

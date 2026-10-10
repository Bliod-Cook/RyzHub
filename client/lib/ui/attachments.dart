import 'dart:async';
import 'dart:math' as math;
import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:cross_file/cross_file.dart';
import 'package:desktop_drop/desktop_drop.dart';
import 'package:file_picker/file_picker.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';

import '../browser.dart';
import '../controller.dart';
import '../l10n.dart';
import '../theme.dart';
import 'common.dart';

const _imageExtensions = {'png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp'};

/// Local thumbnails are read only for images up to this size.
const _thumbnailBytes = 8 * 1024 * 1024;

bool _looksLikeImage(String name) =>
    _imageExtensions.contains(name.split('.').last.toLowerCase());

String fileSize(num bytes) => bytes < 1024
    ? '$bytes B'
    : bytes < 1024 * 1024
    ? '${(bytes / 1024).toStringAsFixed(1)} KB'
    : '${(bytes / 1024 / 1024).toStringAsFixed(1)} MB';

IconData fileIcon(String? kind, String name) {
  final extension = name.split('.').last.toLowerCase();
  if (kind == 'image' || (kind == null && _looksLikeImage(name))) {
    return Icons.image_outlined;
  }
  if (kind == 'pdf' || extension == 'pdf') return Icons.picture_as_pdf_outlined;
  if ({'csv', 'tsv', 'xlsx'}.contains(extension)) {
    return Icons.table_chart_outlined;
  }
  if ({'zip', 'gz', 'tgz', 'tar'}.contains(extension)) {
    return Icons.folder_zip_outlined;
  }
  if (kind == 'text') return Icons.description_outlined;
  return Icons.insert_drive_file_outlined;
}

const _months = [
  'Jan',
  'Feb',
  'Mar',
  'Apr',
  'May',
  'Jun',
  'Jul',
  'Aug',
  'Sep',
  'Oct',
  'Nov',
  'Dec',
];

/// A file's date, e.g. "Sep 18"; the year only when it is not this one.
({String zh, String en})? _fileDate(Object? value, DateTime? now) {
  final t = DateTime.tryParse('$value')?.toLocal();
  if (t == null) return null;
  final thisYear = t.year == (now ?? DateTime.now()).year;
  return (
    zh: '${thisYear ? '' : '${t.year}年'}${t.month}月${t.day}日',
    en: '${_months[t.month - 1]} ${t.day}${thisYear ? '' : ', ${t.year}'}',
  );
}

/// When a file was uploaded, e.g. "Uploaded Sep 18", or made by a model; the
/// year only when it is not this one.
String uploadedOn(Object? value, {DateTime? now, bool generated = false}) {
  final date = _fileDate(value, now);
  if (date == null) return '';
  return generated
      ? tr('AI 生成于 ${date.zh}', 'Made by AI ${date.en}')
      : tr('上传于 ${date.zh}', 'Uploaded ${date.en}');
}

/// When a file was added, to the minute, e.g. "Sep 18, 9:05 PM"; the year
/// only when it is not this one.
String fileTime(Object? value, {DateTime? now}) {
  final date = _fileDate(value, now);
  if (date == null) return '';
  final t = DateTime.parse('$value').toLocal();
  final minute = '${t.minute}'.padLeft(2, '0');
  final hour = t.hour % 12 == 0 ? 12 : t.hour % 12;
  return tr(
    '${date.zh} ${t.hour}:$minute',
    '${date.en}, $hour:$minute ${t.hour < 12 ? 'AM' : 'PM'}',
  );
}

bool _deleted(Object? error) => '$error'.contains('ATTACHMENT_DELETED');

/// A file of this device, ready to upload: [open] reads it from the start
/// each time it is called.
typedef LocalFile = ({
  String name,
  int size,
  Stream<List<int>> Function() open,
});

/// Files picked with the platform's dialog; none if it was dismissed.
Future<List<LocalFile>> chooseLocalFiles() async {
  final files = await FilePicker.pickFiles(
    dialogTitle: tr('选择文件', 'Choose files'),
  );
  final chosen = <LocalFile>[];
  for (final file in files) {
    final size = await file.length();
    if (size != null) {
      chosen.add((name: file.name, size: size, open: file.readAsByteStream));
    } else {
      final bytes = await file.readAsBytes();
      chosen.add((
        name: file.name,
        size: bytes.length,
        open: () => Stream.value(bytes),
      ));
    }
  }
  return chosen;
}

/// Files dropped on the window. Folders are skipped.
Future<List<LocalFile>> droppedFiles(List<XFile> files) async => [
  for (final file in files)
    if (file is! DropItemDirectory)
      (name: file.name, size: await file.length(), open: file.openRead),
];

/// An image small enough to show before it is uploaded.
Future<Uint8List?> _thumbnail(LocalFile file) async {
  if (!_looksLikeImage(file.name) || file.size > _thumbnailBytes) return null;
  final bytes = BytesBuilder(copy: false);
  await for (final chunk in file.open()) {
    bytes.add(chunk);
  }
  return bytes.takeBytes();
}

/// Upload files for the next message; one the library already holds may go
/// as that file instead.
Future<void> attachLocal(
  BuildContext context,
  HubController hub,
  List<LocalFile> files,
) async {
  final prompt = DuplicatePrompt(
    context: context,
    hub: hub,
    many: files.length > 1,
    keep: tr('使用已有文件', 'Use existing'),
  );
  for (final file in files) {
    final thumbnail = await _thumbnail(file);
    unawaited(
      hub.attach(
        file.name,
        file.size,
        file.open,
        thumbnail: thumbnail,
        reuse: (existing) => prompt.reuse(file.name, existing),
      ),
    );
  }
}

/// Pick files with the platform's dialog and upload them for the next message.
Future<void> pickFiles(BuildContext context, HubController hub) async {
  final files = await chooseLocalFiles();
  if (context.mounted) await attachLocal(context, hub, files);
}

/// Upload files dropped on the window for the next message.
Future<void> attachDropped(
  BuildContext context,
  HubController hub,
  List<XFile> files,
) async {
  final dropped = await droppedFiles(files);
  if (context.mounted) await attachLocal(context, hub, dropped);
}

/// Upload bytes already in memory, e.g. a pasted screenshot.
void attachBytes(
  BuildContext context,
  HubController hub,
  String name,
  Uint8List bytes,
) {
  final prompt = DuplicatePrompt(
    context: context,
    hub: hub,
    keep: tr('使用已有文件', 'Use existing'),
  );
  unawaited(
    hub.attach(
      name,
      bytes.length,
      () => Stream.value(bytes),
      thumbnail: _looksLikeImage(name) ? bytes : null,
      reuse: (existing) => prompt.reuse(name, existing),
    ),
  );
}

/// Asks, one file at a time, whether a file the library already holds is
/// uploaded again or the library's file [keep]s its place. With [many] files
/// in the batch, one answer can do for the rest.
class DuplicatePrompt {
  final BuildContext context;
  final HubController hub;
  final bool many;

  /// The label of the answer that takes the library's file.
  final String keep;
  DuplicatePrompt({
    required this.context,
    required this.hub,
    required this.keep,
    this.many = false,
  });

  /// The answer for the rest of the batch, once given.
  bool? _rest;

  /// The question being asked: the next waits for it.
  Future<void> _turn = Future.value();

  /// Whether [name], with the content of the library's [existing], should be
  /// that file rather than uploaded again.
  Future<bool> reuse(String name, Map<String, dynamic> existing) {
    final answer = _turn.then((_) => _ask(name, existing));
    _turn = answer.then((_) {}, onError: (_) {});
    return answer;
  }

  Future<bool> _ask(String name, Map<String, dynamic> existing) async {
    if (_rest != null) return _rest!;
    // Nowhere to ask any more: upload as before.
    if (!context.mounted) return false;
    var rest = false;
    final upload = await showDialog<bool>(
      context: context,
      builder: (context) => StatefulBuilder(
        builder: (context, setState) {
          final c = HubColors.of(context);
          final known = existing['name'] as String;
          final details = [
            uploadedOn(
              existing['created_at'],
              generated: existing['direction'] == 'output',
            ),
            fileSize(existing['size'] as num),
          ].where((part) => part.isNotEmpty).join(' · ');
          return AlertDialog(
            title: Text(tr('文件库里已有相同的文件', 'Already in your library')),
            content: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  children: [
                    FileThumbnail(
                      hub: hub,
                      name: known,
                      id: existing['id'] as String,
                      kind: existing['kind'] as String?,
                      size: 44,
                    ),
                    const SizedBox(width: 14),
                    Flexible(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Text(
                            known,
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                            style: TextStyle(fontSize: 14.5, color: c.text),
                          ),
                          const SizedBox(height: 3),
                          Text(
                            details,
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                            style: TextStyle(fontSize: 12.5, color: c.subtle),
                          ),
                        ],
                      ),
                    ),
                  ],
                ),
                if (name != known) ...[
                  const SizedBox(height: 14),
                  Text(tr('“$name” 与它内容相同。', '"$name" has the same content.')),
                ],
                if (many) ...[
                  const SizedBox(height: 8),
                  CheckboxListTile(
                    contentPadding: EdgeInsets.zero,
                    controlAffinity: ListTileControlAffinity.leading,
                    title: Text(
                      tr('其余重复文件也这样处理', 'Do the same for other duplicates'),
                    ),
                    value: rest,
                    onChanged: (value) => setState(() => rest = value ?? false),
                  ),
                ],
              ],
            ),
            actions: [
              TextButton(
                onPressed: () => Navigator.pop(context, true),
                child: Text(tr('仍然上传', 'Upload anyway')),
              ),
              FilledButton(
                onPressed: () => Navigator.pop(context, false),
                child: Text(keep),
              ),
            ],
          );
        },
      ),
    );
    // Dismissed: the library's file, which uploads nothing.
    final reuse = upload != true;
    if (rest) _rest = reuse;
    return reuse;
  }
}

/// Let the member pick files of their library for the next message.
Future<void> chooseFromLibrary(BuildContext context, HubController hub) async {
  final chosen = await showDialog<List<Map<String, dynamic>>>(
    context: context,
    builder: (context) => _LibraryDialog(hub: hub),
  );
  if (chosen != null && chosen.isNotEmpty) hub.attachFromLibrary(chosen);
}

/// Save a file of the library where the platform keeps downloads.
Future<void> downloadFile(
  BuildContext context,
  HubController hub,
  String id,
  String name,
) async {
  final messenger = ScaffoldMessenger.maybeOf(context);
  try {
    await saveFile(name, await hub.fileContent(id));
  } catch (e) {
    messenger?.showSnackBar(SnackBar(content: Text(errorText(e))));
  }
}

/// A file's picture: local bytes, the server's preview of an image, or the
/// icon of its kind.
class FileThumbnail extends StatelessWidget {
  final HubController hub;
  final String name;
  final String? id, kind;
  final Uint8List? bytes;
  final double size;
  const FileThumbnail({
    super.key,
    required this.hub,
    required this.name,
    this.id,
    this.kind,
    this.bytes,
    this.size = 36,
  });

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final side = (size * MediaQuery.devicePixelRatioOf(context)).round();
    Widget icon(IconData data) => Container(
      width: size,
      height: size,
      decoration: BoxDecoration(
        color: c.hover,
        borderRadius: BorderRadius.circular(8),
      ),
      child: Icon(data, size: size * .55, color: c.muted),
    );
    Widget image(Uint8List data) => ClipRRect(
      borderRadius: BorderRadius.circular(8),
      child: Image(
        image: CoverImage(data, side),
        width: size,
        height: size,
        fit: BoxFit.cover,
        // Decorative: the file's name is always beside it or in a tooltip.
        excludeFromSemantics: true,
        errorBuilder: (_, _, _) => icon(Icons.broken_image_outlined),
      ),
    );
    if (bytes != null) return image(bytes!);
    if (id == null || kind != 'image') return icon(fileIcon(kind, name));
    return FutureBuilder<Uint8List>(
      future: hub.preview(id!),
      builder: (context, snapshot) {
        if (snapshot.hasData && snapshot.data!.isNotEmpty) {
          return image(snapshot.data!);
        }
        if (snapshot.hasError) {
          return icon(
            _deleted(snapshot.error)
                ? Icons.hide_image_outlined
                : Icons.broken_image_outlined,
          );
        }
        return icon(Icons.image_outlined);
      },
    );
  }
}

/// [bytes] decoded just large enough to cover a square of [side] pixels: the
/// shorter edge gets [side], so cropping a wide or tall picture to the square
/// does not magnify it. Never larger than the picture itself.
class CoverImage extends ImageProvider<CoverImage> {
  final Uint8List bytes;
  final int side;
  const CoverImage(this.bytes, this.side);

  @override
  Future<CoverImage> obtainKey(ImageConfiguration configuration) =>
      SynchronousFuture(this);

  @override
  ImageStreamCompleter loadImage(CoverImage key, ImageDecoderCallback decode) =>
      MultiFrameImageStreamCompleter(codec: _decode(decode), scale: 1);

  Future<ui.Codec> _decode(ImageDecoderCallback decode) async => decode(
    await ui.ImmutableBuffer.fromUint8List(bytes),
    getTargetSize: (width, height) {
      final shorter = math.min(width, height);
      if (shorter <= side) return const ui.TargetImageSize();
      return ui.TargetImageSize(
        width: math.max(1, width * side ~/ shorter),
        height: math.max(1, height * side ~/ shorter),
      );
    },
  );

  @override
  bool operator ==(Object other) =>
      other is CoverImage &&
      identical(other.bytes, bytes) &&
      other.side == side;

  @override
  int get hashCode => Object.hash(identityHashCode(bytes), side);
}

/// The files picked for the next message, above the composer's text.
class PendingFiles extends StatelessWidget {
  final HubController hub;
  const PendingFiles({super.key, required this.hub});

  /// Step through the message's uploaded files from [file].
  void preview(PendingFile file) {
    final uploaded = hub.pendingFiles.where((f) => f.id != null).toList();
    hub.openPreview(
      [
        for (final f in uploaded)
          {'id': f.id, 'name': f.name, 'kind': f.kind, 'size': f.size},
      ],
      index: uploaded.indexOf(file),
      inConversation: true,
    );
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Wrap(
      spacing: 6,
      runSpacing: 6,
      children: [
        for (final file in hub.pendingFiles)
          Container(
            key: ObjectKey(file),
            constraints: const BoxConstraints(maxWidth: 240),
            padding: const EdgeInsets.fromLTRB(6, 6, 2, 6),
            decoration: BoxDecoration(
              color: c.background,
              borderRadius: BorderRadius.circular(12),
              border: Border.all(
                color: file.error != null ? c.danger : c.border,
              ),
            ),
            child: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                Flexible(
                  // An uploaded file opens in the preview.
                  child: InkWell(
                    borderRadius: BorderRadius.circular(8),
                    onTap: file.id == null ? null : () => preview(file),
                    child: Row(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        FileThumbnail(
                          hub: hub,
                          name: file.name,
                          id: file.id,
                          kind: file.kind,
                          bytes: file.thumbnail,
                        ),
                        const SizedBox(width: 8),
                        Flexible(
                          child: Column(
                            crossAxisAlignment: CrossAxisAlignment.start,
                            mainAxisSize: MainAxisSize.min,
                            children: [
                              Text(
                                file.name,
                                maxLines: 1,
                                overflow: TextOverflow.ellipsis,
                                style: TextStyle(fontSize: 13, color: c.text),
                              ),
                              Text(
                                file.error != null
                                    ? errorText(file.error!)
                                    : file.uploading
                                    ? tr('上传中…', 'Uploading…')
                                    : fileSize(file.size),
                                maxLines: 1,
                                overflow: TextOverflow.ellipsis,
                                style: TextStyle(
                                  fontSize: 11.5,
                                  color: file.error != null
                                      ? c.danger
                                      : c.subtle,
                                ),
                              ),
                            ],
                          ),
                        ),
                      ],
                    ),
                  ),
                ),
                if (file.uploading)
                  const Padding(
                    padding: EdgeInsets.symmetric(horizontal: 8),
                    child: SizedBox(
                      width: 14,
                      height: 14,
                      child: CircularProgressIndicator(strokeWidth: 2),
                    ),
                  ),
                IconButton(
                  tooltip: tr('移除', 'Remove'),
                  visualDensity: VisualDensity.compact,
                  iconSize: 16,
                  onPressed: () => hub.detach(file),
                  icon: Icon(Icons.close, color: c.muted),
                ),
              ],
            ),
          ),
      ],
    );
  }
}

/// The files a message carried: images as pictures, other files as chips.
class MessageFiles extends StatelessWidget {
  final HubController hub;
  final List<dynamic> files;
  final WrapAlignment alignment;
  const MessageFiles({
    super.key,
    required this.hub,
    required this.files,
    this.alignment = WrapAlignment.end,
  });

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final entries = [
      for (final raw in files)
        if (raw is Map) Map<String, dynamic>.from(raw),
    ];
    return Wrap(
      alignment: alignment,
      spacing: 8,
      runSpacing: 8,
      children: [
        for (final (i, file) in entries.indexed)
          Builder(
            builder: (context) {
              final id = '${file['id'] ?? ''}';
              final name = '${file['name'] ?? file['path'] ?? ''}';
              final kind = file['kind'] as String?;
              void open() =>
                  hub.openPreview(entries, index: i, inConversation: true);
              if (kind == 'image') {
                return Tooltip(
                  message: name,
                  child: InkWell(
                    borderRadius: BorderRadius.circular(8),
                    onTap: open,
                    child: FileThumbnail(
                      hub: hub,
                      name: name,
                      id: id,
                      kind: kind,
                      size: 120,
                    ),
                  ),
                );
              }
              return InkWell(
                borderRadius: BorderRadius.circular(12),
                onTap: open,
                child: Container(
                  constraints: const BoxConstraints(maxWidth: 260),
                  padding: const EdgeInsets.fromLTRB(8, 8, 12, 8),
                  decoration: BoxDecoration(
                    borderRadius: BorderRadius.circular(12),
                    border: Border.all(color: c.border),
                  ),
                  child: Row(
                    mainAxisSize: MainAxisSize.min,
                    children: [
                      FileThumbnail(hub: hub, name: name, kind: kind),
                      const SizedBox(width: 8),
                      Flexible(
                        child: Column(
                          crossAxisAlignment: CrossAxisAlignment.start,
                          mainAxisSize: MainAxisSize.min,
                          children: [
                            Text(
                              name,
                              maxLines: 1,
                              overflow: TextOverflow.ellipsis,
                              style: TextStyle(fontSize: 13, color: c.text),
                            ),
                            Text(
                              fileSize((file['size'] as num?) ?? 0),
                              style: TextStyle(fontSize: 11.5, color: c.subtle),
                            ),
                          ],
                        ),
                      ),
                    ],
                  ),
                ),
              );
            },
          ),
      ],
    );
  }
}

/// A search box over the library. [onSearch] runs once typing pauses, or at
/// once on submit.
class FileSearchField extends StatefulWidget {
  final ValueChanged<String> onSearch;
  final bool autofocus;
  const FileSearchField({
    super.key,
    required this.onSearch,
    this.autofocus = false,
  });
  @override
  State<FileSearchField> createState() => _FileSearchFieldState();
}

class _FileSearchFieldState extends State<FileSearchField> {
  final text = TextEditingController();
  Timer? pause;

  void search(String query, {bool now = false}) {
    pause?.cancel();
    if (now) {
      widget.onSearch(query);
    } else {
      pause = Timer(
        const Duration(milliseconds: 300),
        () => widget.onSearch(query),
      );
    }
  }

  @override
  void dispose() {
    pause?.cancel();
    text.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final border = OutlineInputBorder(
      borderRadius: BorderRadius.circular(999),
      borderSide: BorderSide(color: c.border),
    );
    return SizedBox(
      height: 40,
      // Filling the box keeps one height with or without the clear button.
      child: TextField(
        controller: text,
        autofocus: widget.autofocus,
        expands: true,
        maxLines: null,
        textAlignVertical: TextAlignVertical.center,
        onChanged: (query) {
          setState(() {});
          search(query);
        },
        onSubmitted: (query) => search(query, now: true),
        textInputAction: TextInputAction.search,
        style: const TextStyle(fontSize: 14),
        decoration: InputDecoration(
          hintText: tr('搜索文件库', 'Search library'),
          prefixIcon: Icon(Icons.search, size: 20, color: c.muted),
          prefixIconConstraints: const BoxConstraints(minWidth: 42),
          suffixIcon: text.text.isEmpty
              ? null
              : IconButton(
                  tooltip: tr('清除', 'Clear'),
                  iconSize: 16,
                  visualDensity: VisualDensity.compact,
                  onPressed: () {
                    text.clear();
                    setState(() {});
                    search('', now: true);
                  },
                  icon: Icon(Icons.close, color: c.muted),
                ),
          isDense: true,
          contentPadding: EdgeInsets.zero,
          border: border,
          enabledBorder: border,
          focusedBorder: border.copyWith(
            borderSide: BorderSide(color: c.subtle),
          ),
        ),
      ),
    );
  }
}

class _LibraryDialog extends StatefulWidget {
  final HubController hub;
  const _LibraryDialog({required this.hub});
  @override
  State<_LibraryDialog> createState() => _LibraryDialogState();
}

class _LibraryDialogState extends State<_LibraryDialog> {
  List<Map<String, dynamic>> files = [];

  /// Picked files by id, kept while a search hides them.
  final Map<String, Map<String, dynamic>> chosen = {};
  bool busy = true;
  String error = '', query = '';
  int _loads = 0;

  int get room => widget.hub.maxFiles - widget.hub.pendingFiles.length;

  @override
  void initState() {
    super.initState();
    load();
  }

  Future<void> load() async {
    final load = ++_loads;
    setState(() => busy = true);
    try {
      final files = await widget.hub.library(query: query);
      if (!mounted || load != _loads) return;
      setState(() {
        this.files = files;
        error = '';
        busy = false;
      });
    } catch (e) {
      if (!mounted || load != _loads) return;
      setState(() {
        error = errorText(e);
        busy = false;
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final full = chosen.length >= room;
    return Dialog(
      insetPadding: const EdgeInsets.all(24),
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 600, maxHeight: 620),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(24, 16, 12, 12),
              child: Row(
                children: [
                  Expanded(
                    child: Text(
                      tr('从文件库添加', 'Add from library'),
                      style: TextStyle(
                        fontSize: 20,
                        fontWeight: FontWeight.w600,
                        color: c.text,
                      ),
                    ),
                  ),
                  IconButton(
                    tooltip: tr('关闭', 'Close'),
                    icon: const Icon(Icons.close),
                    onPressed: () => Navigator.pop(context),
                  ),
                ],
              ),
            ),
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 24),
              child: FileSearchField(
                autofocus: true,
                onSearch: (query) {
                  this.query = query;
                  load();
                },
              ),
            ),
            const SizedBox(height: 8),
            Expanded(
              child: busy
                  ? const Center(child: CircularProgressIndicator())
                  : error.isNotEmpty
                  ? Center(child: Text(error))
                  : files.isEmpty
                  ? Center(
                      child: Text(
                        query.trim().isEmpty
                            ? tr('还没有上传过文件', 'No files uploaded yet')
                            : tr('没有匹配的文件', 'No matching files'),
                        style: TextStyle(color: c.subtle),
                      ),
                    )
                  : ListView(
                      padding: const EdgeInsets.fromLTRB(14, 4, 14, 8),
                      children: [
                        for (final file in files)
                          _LibraryRow(
                            key: ValueKey(file['id']),
                            hub: widget.hub,
                            file: file,
                            chosen: chosen.containsKey(file['id']),
                            onTap: chosen.containsKey(file['id']) || !full
                                ? () => setState(
                                    () => chosen.remove(file['id']) == null
                                        ? chosen[file['id'] as String] = file
                                        : null,
                                  )
                                : null,
                          ),
                      ],
                    ),
            ),
            // The actions show once a file is picked.
            AnimatedSize(
              duration: const Duration(milliseconds: 180),
              curve: Curves.easeOut,
              alignment: Alignment.topCenter,
              child: chosen.isEmpty
                  ? const SizedBox(height: 12)
                  : Container(
                      decoration: BoxDecoration(
                        border: Border(top: BorderSide(color: c.border)),
                      ),
                      padding: const EdgeInsets.fromLTRB(24, 12, 20, 16),
                      child: Row(
                        children: [
                          Expanded(
                            child: Text(
                              tr(
                                    '已选 ${chosen.length} 个',
                                    '${chosen.length} selected',
                                  ) +
                                  (full
                                      ? tr(' · 已达上限', ' · limit reached')
                                      : ''),
                              maxLines: 1,
                              overflow: TextOverflow.ellipsis,
                              style: TextStyle(fontSize: 13.5, color: c.subtle),
                            ),
                          ),
                          TextButton(
                            onPressed: () => Navigator.pop(context),
                            child: Text(tr('取消', 'Cancel')),
                          ),
                          const SizedBox(width: 8),
                          FilledButton(
                            onPressed: () =>
                                Navigator.pop(context, chosen.values.toList()),
                            child: Text(tr('添加到对话', 'Add to chat')),
                          ),
                        ],
                      ),
                    ),
            ),
          ],
        ),
      ),
    );
  }
}

/// A file of the library dialog: tapping picks it or puts it back; [onTap] is
/// null while no more files fit the message.
class _LibraryRow extends StatelessWidget {
  final HubController hub;
  final Map<String, dynamic> file;
  final bool chosen;
  final VoidCallback? onTap;
  const _LibraryRow({
    super.key,
    required this.hub,
    required this.file,
    required this.chosen,
    required this.onTap,
  });

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final name = file['name'] as String;
    final details = [
      fileTime(file['created_at']),
      fileSize(file['size'] as num),
    ].where((part) => part.isNotEmpty).join(' · ');
    return Semantics(
      button: true,
      selected: chosen,
      enabled: onTap != null,
      child: Opacity(
        opacity: onTap == null ? .45 : 1,
        child: Hover(
          builder: (context, hovered) => Material(
            color: chosen
                ? c.selected
                : hovered && onTap != null
                ? c.hover
                : Colors.transparent,
            borderRadius: BorderRadius.circular(12),
            child: InkWell(
              borderRadius: BorderRadius.circular(12),
              onTap: onTap,
              child: Padding(
                padding: const EdgeInsets.symmetric(
                  horizontal: 10,
                  vertical: 9,
                ),
                child: Row(
                  children: [
                    FileThumbnail(
                      hub: hub,
                      name: name,
                      id: file['id'] as String,
                      kind: file['kind'] as String?,
                      size: 44,
                    ),
                    const SizedBox(width: 14),
                    Expanded(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Text(
                            name,
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                            style: TextStyle(fontSize: 14.5, color: c.text),
                          ),
                          const SizedBox(height: 3),
                          Text(
                            details,
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                            style: TextStyle(fontSize: 12.5, color: c.subtle),
                          ),
                        ],
                      ),
                    ),
                    const SizedBox(width: 12),
                    // Keeps its place so the text does not shift on picking.
                    SizedBox(
                      width: 22,
                      height: 22,
                      child: chosen
                          ? DecoratedBox(
                              decoration: BoxDecoration(
                                color: c.text,
                                shape: BoxShape.circle,
                              ),
                              child: Icon(
                                Icons.check,
                                size: 15,
                                color: c.background,
                              ),
                            )
                          : null,
                    ),
                    const SizedBox(width: 4),
                  ],
                ),
              ),
            ),
          ),
        ),
      ),
    );
  }
}

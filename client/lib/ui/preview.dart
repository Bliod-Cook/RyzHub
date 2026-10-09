import 'dart:async';
import 'dart:convert';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:pdfrx/pdfrx.dart';

import '../controller.dart';
import '../l10n.dart';
import '../prefs.dart';
import '../theme.dart';
import 'attachments.dart';
import 'code.dart';
import 'common.dart';
import 'composer.dart';
import 'messages.dart';

/// Text files are previewed up to this size.
const previewTextBytes = 1024 * 1024;

/// Extensions of text files the server may not have recognised as text,
/// e.g. because they are larger than it inlines.
const _textExtensions = {
  'txt', 'log', 'md', 'markdown', 'csv', 'tsv', 'json', 'jsonl', 'yaml', //
  'yml', 'toml', 'ini', 'cfg', 'conf', 'env', 'xml', 'html', 'htm', 'svg',
  'css', 'scss', 'less', 'js', 'mjs', 'cjs', 'jsx', 'ts', 'tsx', 'vue',
  'svelte', 'py', 'rb', 'php', 'pl', 'lua', 'r', 'rs', 'go', 'java', 'kt',
  'kts', 'scala', 'swift', 'dart', 'c', 'h', 'cc', 'cpp', 'hpp', 'cs', 'm',
  'sh', 'bash', 'zsh', 'fish', 'ps1', 'bat', 'sql', 'graphql', 'proto',
  'gradle', 'cmake', 'mk', 'tex', 'rst', 'adoc', 'diff', 'patch', 'srt',
};

const _textTypes = {
  'application/json',
  'application/yaml',
  'application/xml',
  'image/svg+xml',
};

/// A file as the preview sees it, from a library entry or the attachment of
/// a message.
class PreviewFile {
  final String id, name;
  final String? kind, mediaType;
  final int size;
  const PreviewFile({
    required this.id,
    required this.name,
    required this.size,
    this.kind,
    this.mediaType,
  });

  factory PreviewFile.of(Map<String, dynamic> file) => PreviewFile(
    id: '${file['id'] ?? ''}',
    name: '${file['name'] ?? file['path'] ?? ''}',
    kind: file['kind'] as String?,
    mediaType: file['media_type'] as String?,
    size: (file['size'] as num?)?.toInt() ?? 0,
  );

  String get extension =>
      name.contains('.') ? name.split('.').last.toLowerCase() : '';
}

/// Shows one kind of file. The preview asks each of [previewers] in turn,
/// and the first that accepts a file shows it.
abstract class FilePreviewer {
  const FilePreviewer();
  bool accepts(PreviewFile file);
  Widget build(BuildContext context, HubController hub, PreviewFile file);
}

/// Renderers of HTML or SVG belong before [TextPreviewer], which shows their
/// source until then.
const previewers = <FilePreviewer>[
  ImagePreviewer(),
  PdfPreviewer(),
  TextPreviewer(),
];

/// What shows [file], or null when nothing can.
FilePreviewer? previewerFor(PreviewFile file) {
  for (final previewer in previewers) {
    if (previewer.accepts(file)) return previewer;
  }
  return null;
}

/// Around a preview floating over the conversation: a tap on the backdrop,
/// anywhere but on a [PreviewContent], closes it. A previewer that draws its
/// own space around the file, such as between a PDF's pages, closes it with
/// [of] instead.
class PreviewBackdrop extends InheritedWidget {
  final VoidCallback onTap;
  const PreviewBackdrop({super.key, required this.onTap, required super.child});

  static VoidCallback? of(BuildContext context) =>
      context.dependOnInheritedWidgetOfExactType<PreviewBackdrop>()?.onTap;

  @override
  bool updateShouldNotify(PreviewBackdrop old) => old.onTap != onTap;
}

/// The file a previewer shows, as opposed to the backdrop around it.
class PreviewContent extends StatelessWidget {
  final Widget child;
  const PreviewContent({super.key, required this.child});
  @override
  Widget build(BuildContext context) => PreviewBackdrop.of(context) == null
      ? child
      // Taken here, a tap does not reach the backdrop.
      : GestureDetector(
          behavior: HitTestBehavior.opaque,
          excludeFromSemantics: true,
          onTap: () {},
          child: child,
        );
}

/// Types the server gives only to files whose bytes are a picture. One it
/// could not render at upload is a file of such a type, and may have been
/// rendered since; a message keeps the kind it was sent with.
const _pictureTypes = {
  'image/png',
  'image/jpeg',
  'image/gif',
  'image/webp',
  'image/bmp',
};

/// Originals up to this size replace the rendition once they arrive.
const previewOriginalBytes = 20 * 1024 * 1024;

/// The longest side an original is decoded at, to bound memory.
const _originalSide = 8192;

/// A picture to zoom and pan: the server's rendition at once, then the file
/// as uploaded, at full size and animated, when it is small enough to fetch.
class ImagePreviewer extends FilePreviewer {
  const ImagePreviewer();
  @override
  bool accepts(PreviewFile file) =>
      file.kind == 'image' ||
      (file.kind == 'file' && _pictureTypes.contains(file.mediaType));
  @override
  Widget build(BuildContext context, HubController hub, PreviewFile file) =>
      _ImagePreview(hub: hub, file: file);
}

class _ImagePreview extends StatefulWidget {
  final HubController hub;
  final PreviewFile file;
  const _ImagePreview({required this.hub, required this.file});
  @override
  State<_ImagePreview> createState() => _ImagePreviewState();
}

class _ImagePreviewState extends State<_ImagePreview> {
  Uint8List? rendition, original;
  bool renditionDone = false, originalDone = false;

  /// The original would not decode; the rendition stays.
  bool originalBroken = false;
  Object? error;

  @override
  void initState() {
    super.initState();
    final PreviewFile(:id, :size) = widget.file;
    widget.hub
        .preview(id)
        .then(
          (bytes) => _set(() {
            if (bytes.isNotEmpty) rendition = bytes;
            renditionDone = true;
          }),
          // No rendition is no preview, not a failure: the original may do.
          onError: (Object e) => _set(() {
            if (!'$e'.contains('ATTACHMENT_UNAVAILABLE')) error ??= e;
            renditionDone = true;
          }),
        );
    if (size > previewOriginalBytes) {
      originalDone = true;
    } else {
      widget.hub
          .original(id)
          .then(
            (bytes) => _set(() {
              original = bytes;
              originalDone = true;
            }),
            onError: (Object e) => _set(() {
              error ??= e;
              originalDone = true;
            }),
          );
    }
  }

  void _set(VoidCallback change) {
    if (mounted) setState(change);
  }

  @override
  Widget build(BuildContext context) {
    final hub = widget.hub, file = widget.file;
    final deleted = '$error'.contains('ATTACHMENT_DELETED');
    final shown = deleted ? null : (originalBroken ? null : original);
    final ImageProvider? image = shown != null
        ? ResizeImage(
            MemoryImage(shown),
            width: _originalSide,
            height: _originalSide,
            policy: ResizeImagePolicy.fit,
          )
        : rendition != null && !deleted
        ? MemoryImage(rendition!)
        : null;
    if (image == null) {
      if (!deleted && !(renditionDone && originalDone)) {
        return const Center(child: CircularProgressIndicator());
      }
      return UnavailablePreview(
        hub: hub,
        file: file,
        deleted: deleted,
        reason: error == null ? null : errorText(error!),
      );
    }
    return InteractiveViewer(
      maxScale: 8,
      child: Center(
        child: Padding(
          padding: const EdgeInsets.all(24),
          child: PreviewContent(
            child: DecoratedBox(
              // Lifted off the blurred page, which may be as white.
              decoration: BoxDecoration(
                boxShadow: [
                  if (PreviewBackdrop.of(context) != null)
                    BoxShadow(
                      color: Colors.black.withValues(alpha: .14),
                      blurRadius: 28,
                      offset: const Offset(0, 6),
                    ),
                ],
              ),
              child: Image(
                image: image,
                fit: BoxFit.contain,
                semanticLabel: file.name,
                // The rendition stays on screen while the original decodes.
                gaplessPlayback: true,
                errorBuilder: (context, _, _) {
                  if (shown == null) {
                    return UnavailablePreview(hub: hub, file: file);
                  }
                  // Back to the rendition, or to the notice without one.
                  WidgetsBinding.instance.addPostFrameCallback(
                    (_) => _set(() => originalBroken = true),
                  );
                  return const SizedBox.shrink();
                },
              ),
            ),
          ),
        ),
      ),
    );
  }
}

/// A PDF's pages, drawn by PDFium on every platform.
class PdfPreviewer extends FilePreviewer {
  const PdfPreviewer();
  @override
  bool accepts(PreviewFile file) => file.kind == 'pdf';
  @override
  Widget build(BuildContext context, HubController hub, PreviewFile file) {
    final c = HubColors.of(context);
    final backdrop = PreviewBackdrop.of(context);
    return _Loaded<Uint8List>(
      hub: hub,
      file: file,
      load: () => hub.original(file.id),
      builder: (context, bytes) => PdfViewer.data(
        bytes,
        sourceName: 'attachment:${file.id}',
        params: PdfViewerParams(
          backgroundColor: backdrop == null ? c.sidebar : Colors.transparent,
          margin: 16,
          // Beside and between the pages is the backdrop.
          onGeneralTap: (context, controller, details) {
            if (backdrop == null ||
                details.type != PdfViewerGeneralTapType.tap ||
                controller.layout.pageLayouts.any(
                  (page) => page.contains(details.documentPosition),
                )) {
              return false;
            }
            backdrop();
            return true;
          },
          errorBannerBuilder: (context, _, _, _) => UnavailablePreview(
            hub: hub,
            file: file,
            reason: tr('无法打开这个 PDF', 'This PDF could not be opened'),
          ),
        ),
      ),
    );
  }
}

/// Text as written, in an even-width font; Markdown as messages show it.
class TextPreviewer extends FilePreviewer {
  const TextPreviewer();
  @override
  bool accepts(PreviewFile file) {
    if (file.kind == 'text') return true;
    if (file.kind != 'file' || file.size > previewTextBytes) return false;
    final type = file.mediaType ?? '';
    return type.startsWith('text/') ||
        _textTypes.contains(type) ||
        _textExtensions.contains(file.extension);
  }

  bool _markdown(PreviewFile file) =>
      file.mediaType == 'text/markdown' ||
      file.extension == 'md' ||
      file.extension == 'markdown';

  @override
  Widget build(BuildContext context, HubController hub, PreviewFile file) =>
      _Loaded<String?>(
        hub: hub,
        file: file,
        load: () async {
          final bytes = await hub.original(file.id);
          try {
            return utf8.decode(bytes);
          } on FormatException {
            return null;
          }
        },
        builder: (context, text) => text == null
            ? UnavailablePreview(
                hub: hub,
                file: file,
                reason: tr(
                  '无法预览：不是文本文件',
                  'Preview not available: not a text file',
                ),
              )
            : _markdown(file)
            ? _Paper(maxWidth: contentWidth + 64, child: MarkdownText(text))
            : _Paper(child: _Code(text, language: file.extension)),
      );
}

/// Text on a page of its own, centred over the preview's background.
class _Paper extends StatelessWidget {
  final double maxWidth;
  final Widget child;
  const _Paper({required this.child, this.maxWidth = 960});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return SingleChildScrollView(
      padding: const EdgeInsets.fromLTRB(16, 8, 16, 24),
      child: Center(
        child: PreviewContent(
          child: Container(
            width: double.infinity,
            constraints: BoxConstraints(maxWidth: maxWidth),
            padding: const EdgeInsets.fromLTRB(32, 28, 32, 32),
            decoration: BoxDecoration(
              color: c.background,
              borderRadius: BorderRadius.circular(16),
              border: Border.all(color: c.border),
            ),
            child: child,
          ),
        ),
      ),
    );
  }
}

/// Source text: long lines scroll sideways rather than wrap. [language] is
/// the file's extension.
class _Code extends StatelessWidget {
  final String text;
  final String language;
  const _Code(this.text, {required this.language});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return SingleChildScrollView(
      scrollDirection: Axis.horizontal,
      child: CodeText(
        text,
        language: language,
        style: TextStyle(
          fontFamily: '0xProto',
          fontFamilyFallback: const [fontFamily, ...fontFallback],
          fontSize: 13,
          height: 1.55,
          color: c.text,
        ),
      ),
    );
  }
}

/// A file nothing here can show: its name, why, and a download.
class UnavailablePreview extends StatelessWidget {
  final HubController hub;
  final PreviewFile file;
  final String? reason;

  /// The file is gone, so there is nothing to download either.
  final bool deleted;
  const UnavailablePreview({
    super.key,
    required this.hub,
    required this.file,
    this.reason,
    this.deleted = false,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Center(
      child: SingleChildScrollView(
        padding: const EdgeInsets.all(24),
        child: PreviewContent(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Container(
                width: 80,
                height: 80,
                decoration: BoxDecoration(
                  color: c.hover,
                  borderRadius: BorderRadius.circular(20),
                ),
                child: Icon(
                  fileIcon(file.kind, file.name),
                  size: 32,
                  color: c.muted,
                ),
              ),
              const SizedBox(height: 20),
              Text(
                file.name,
                textAlign: TextAlign.center,
                style: TextStyle(fontSize: 16, color: c.text),
              ),
              const SizedBox(height: 6),
              Text(
                deleted
                    ? tr('文件已被删除', 'This file was deleted')
                    : reason ?? tr('无法预览', 'Preview not available'),
                textAlign: TextAlign.center,
                style: TextStyle(fontSize: 14, color: c.subtle),
              ),
              if (!deleted) ...[
                const SizedBox(height: 4),
                Text(
                  tr(
                    '此类文件可以下载，或附加到对话中。',
                    'This file can be downloaded or attached to a chat.',
                  ),
                  textAlign: TextAlign.center,
                  style: TextStyle(fontSize: 14, color: c.subtle),
                ),
                const SizedBox(height: 16),
                OutlinedButton(
                  style: OutlinedButton.styleFrom(
                    visualDensity: VisualDensity.compact,
                    padding: const EdgeInsets.symmetric(horizontal: 14),
                    shape: const StadiumBorder(),
                    side: BorderSide(color: c.border),
                    backgroundColor: c.background,
                  ),
                  onPressed: () =>
                      downloadFile(context, hub, file.id, file.name),
                  child: Text(tr('下载文件', 'Download file')),
                ),
              ],
            ],
          ),
        ),
      ),
    );
  }
}

/// Loads what a previewer shows once, with a spinner meanwhile; a failure
/// falls back to [UnavailablePreview].
class _Loaded<T> extends StatefulWidget {
  final HubController hub;
  final PreviewFile file;
  final Future<T> Function() load;
  final Widget Function(BuildContext context, T value) builder;
  const _Loaded({
    super.key,
    required this.hub,
    required this.file,
    required this.load,
    required this.builder,
  });
  @override
  State<_Loaded<T>> createState() => _LoadedState<T>();
}

class _LoadedState<T> extends State<_Loaded<T>> {
  late final Future<T> future = widget.load();
  @override
  Widget build(BuildContext context) => FutureBuilder<T>(
    future: future,
    builder: (context, snapshot) {
      if (snapshot.hasError) {
        final error = snapshot.error!;
        final deleted = '$error'.contains('ATTACHMENT_DELETED');
        return UnavailablePreview(
          hub: widget.hub,
          file: widget.file,
          deleted: deleted,
          reason: errorText(error),
        );
      }
      if (snapshot.connectionState != ConnectionState.done) {
        return const Center(child: CircularProgressIndicator());
      }
      return widget.builder(context, snapshot.data as T);
    },
  );
}

/// A file's preview: its name, download and close above it, a question
/// about it below. Opaque over the main area; or [floating] over the whole
/// page, which shows blurred around the file, and a tap there closes it.
class FilePreviewView extends StatefulWidget {
  final HubController hub;

  /// Ask the question about the file: in a new chat, or, floating, in the
  /// conversation underneath. A failure puts the question back.
  final Future<void> Function(Map<String, dynamic> file, String question) onAsk;
  final bool floating;

  /// The conversation's run, for its questions, as its composer has them.
  final bool running, steer;
  final VoidCallback? onCancel;
  const FilePreviewView({
    super.key,
    required this.hub,
    required this.onAsk,
    this.floating = false,
    this.running = false,
    this.steer = false,
    this.onCancel,
  });
  @override
  State<FilePreviewView> createState() => _FilePreviewViewState();
}

class _FilePreviewViewState extends State<FilePreviewView> {
  final focus = FocusNode(debugLabel: 'file preview');

  HubController get hub => widget.hub;

  @override
  void initState() {
    super.initState();
    // Take the keys from the composer underneath, which autofocus would not.
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) focus.requestFocus();
    });
  }

  @override
  void dispose() {
    focus.dispose();
    super.dispose();
  }

  void step(int by) => hub.previewAt(hub.previewIndex + by);

  /// Keys the question field leaves alone: arrows step, Escape closes.
  KeyEventResult key(FocusNode node, KeyEvent event) {
    if (event is! KeyDownEvent && event is! KeyRepeatEvent) {
      return KeyEventResult.ignored;
    }
    switch (event.logicalKey) {
      case LogicalKeyboardKey.escape:
        hub.closePreview();
      case LogicalKeyboardKey.arrowLeft:
        step(-1);
      case LogicalKeyboardKey.arrowRight:
        step(1);
      default:
        return KeyEventResult.ignored;
    }
    return KeyEventResult.handled;
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final files = hub.previewFiles ?? const [];
    if (files.isEmpty) return const SizedBox.shrink();
    final index = hub.previewIndex.clamp(0, files.length - 1);
    final raw = files[index];
    final file = PreviewFile.of(raw);
    final body =
        previewerFor(file)?.build(context, hub, file) ??
        UnavailablePreview(hub: hub, file: file);
    final close = hub.closePreview;
    Widget stage = Hover(
      builder: (context, hovered) => Stack(
        children: [
          Positioned.fill(
            child: KeyedSubtree(
              key: ValueKey('preview:${file.id}'),
              child: body,
            ),
          ),
          if (index > 0)
            _Step(
              key: const ValueKey('previous'),
              left: true,
              visible: hovered,
              onPressed: () => step(-1),
            ),
          if (index < files.length - 1)
            _Step(
              key: const ValueKey('next'),
              left: false,
              visible: hovered,
              onPressed: () => step(1),
            ),
        ],
      ),
    );
    if (widget.floating) {
      stage = GestureDetector(
        behavior: HitTestBehavior.opaque,
        excludeFromSemantics: true,
        onTap: close,
        child: PreviewBackdrop(onTap: close, child: stage),
      );
    }
    final page = SafeArea(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(12, 10, 12, 6),
            child: Row(
              children: [
                Flexible(
                  child: Align(
                    alignment: Alignment.centerLeft,
                    child: _Raised(
                      child: Padding(
                        padding: const EdgeInsets.fromLTRB(14, 0, 18, 0),
                        child: Row(
                          mainAxisSize: MainAxisSize.min,
                          children: [
                            Icon(
                              fileIcon(file.kind, file.name),
                              size: 18,
                              color: c.text,
                            ),
                            const SizedBox(width: 8),
                            Flexible(
                              child: Semantics(
                                header: true,
                                child: Text(
                                  file.name,
                                  maxLines: 1,
                                  overflow: TextOverflow.ellipsis,
                                  style: TextStyle(fontSize: 15, color: c.text),
                                ),
                              ),
                            ),
                          ],
                        ),
                      ),
                    ),
                  ),
                ),
                const SizedBox(width: 12),
                _Raised(
                  round: true,
                  child: IconButton(
                    tooltip: tr('下载', 'Download'),
                    icon: const Icon(Icons.download_outlined, size: 20),
                    onPressed: () =>
                        downloadFile(context, hub, file.id, file.name),
                  ),
                ),
                const SizedBox(width: 8),
                _Raised(
                  round: true,
                  child: IconButton(
                    tooltip: tr('关闭', 'Close'),
                    icon: const Icon(Icons.close, size: 20),
                    onPressed: close,
                  ),
                ),
              ],
            ),
          ),
          Expanded(child: stage),
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 6, 16, 14),
            child: Center(
              child: ConstrainedBox(
                constraints: const BoxConstraints(maxWidth: contentWidth),
                child: Composer(
                  key: ValueKey('ask:${file.id}'),
                  hint: widget.floating
                      ? tr('就这个文件继续提问', 'Follow up about this file')
                      : tr('就这个文件提问', 'Ask about this file'),
                  enterToSend: prefs.enterToSend,
                  running: widget.running,
                  steer: widget.steer,
                  onCancel: widget.onCancel ?? () {},
                  onSend: (question) => widget.onAsk(raw, question),
                ),
              ),
            ),
          ),
        ],
      ),
    );
    return PopScope(
      canPop: false,
      onPopInvokedWithResult: (popped, _) {
        if (!popped) close();
      },
      child: Focus(
        focusNode: focus,
        autofocus: true,
        onKeyEvent: key,
        child: widget.floating
            ? Material(
                type: MaterialType.transparency,
                child: Stack(
                  fit: StackFit.expand,
                  children: [
                    // The page underneath, blurred; a tap on it closes.
                    GestureDetector(
                      behavior: HitTestBehavior.opaque,
                      excludeFromSemantics: true,
                      onTap: close,
                      child: ClipRect(
                        child: BackdropFilter(
                          filter: ui.ImageFilter.blur(sigmaX: 24, sigmaY: 24),
                          child: ColoredBox(
                            color: c.background.withValues(alpha: .72),
                          ),
                        ),
                      ),
                    ),
                    page,
                  ],
                ),
              )
            : Material(color: c.sidebar, child: page),
      ),
    );
  }
}

/// A white control floating over the preview: the name's pill or a round
/// button.
class _Raised extends StatelessWidget {
  final Widget child;
  final bool round;
  const _Raised({required this.child, this.round = false});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Container(
      height: 44,
      width: round ? 44 : null,
      decoration: BoxDecoration(
        color: c.background,
        borderRadius: BorderRadius.circular(22),
        border: Border.all(color: c.border),
        boxShadow: [
          BoxShadow(
            color: Colors.black.withValues(alpha: .06),
            blurRadius: 8,
            offset: const Offset(0, 1),
          ),
        ],
      ),
      // An alignment would stretch the name's pill across the bar.
      alignment: round ? Alignment.center : null,
      child: child,
    );
  }
}

/// The previous or next file, at the side of the preview.
class _Step extends StatelessWidget {
  final bool left, visible;
  final VoidCallback onPressed;
  const _Step({
    super.key,
    required this.left,
    required this.visible,
    required this.onPressed,
  });
  @override
  Widget build(BuildContext context) => Positioned(
    left: left ? 16 : null,
    right: left ? null : 16,
    top: 0,
    bottom: 0,
    child: Center(
      child: Reveal(
        visible: visible,
        child: _Raised(
          round: true,
          child: IconButton(
            tooltip: left ? tr('上一个', 'Previous') : tr('下一个', 'Next'),
            icon: Icon(left ? Icons.chevron_left : Icons.chevron_right),
            onPressed: onPressed,
          ),
        ),
      ),
    ),
  );
}

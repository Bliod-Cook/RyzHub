import 'dart:convert';

import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter_math_fork/flutter_math.dart';

import '../rust/api.dart' as rust;
import '../theme.dart';
import 'code.dart';

/// Text the kernel rendered: its blocks, and the runs of cited sources its chips stand for,
/// each as `{label, url}`.
class Rendered {
  final List<dynamic> blocks;
  final List<dynamic> chips;
  const Rendered(this.blocks, this.chips);

  static final _cache = <(String, String, bool), Rendered>{};

  /// [text] rendered whole, for text shown outside a timeline: [cited] are the pages a reply
  /// cites, and [streaming] closes what its end leaves open. The latest are kept.
  factory Rendered.of(
    String text, {
    List<String> cited = const [],
    bool streaming = false,
  }) {
    final key = (text, cited.join('\n'), streaming);
    final hit = _cache.remove(key);
    if (hit != null) return _cache[key] = hit;
    final json = jsonDecode(
      rust.renderMarkdown(text: text, cited: cited, streaming: streaming),
    );
    final rendered = Rendered(json['blocks'] as List, json['chips'] as List);
    _cache[key] = rendered;
    if (_cache.length > 64) _cache.remove(_cache.keys.first);
    return rendered;
  }
}

/// Space between blocks.
const _gap = 8.0;

/// Blocks the kernel rendered. A block the kernel has not changed keeps its widget, so a
/// streaming message rebuilds only its last block.
class MarkdownBlocks extends StatefulWidget {
  final List<dynamic> blocks;
  final double size;
  final Color? color;

  /// Opens a link: a page or a conversation.
  final ValueChanged<String>? onLink;

  /// Opens a file of the sandbox a reply links to, by its path as the server
  /// records it.
  final ValueChanged<String>? onFile;

  /// The chip for a run of cited sources, by its index.
  final Widget? Function(int index)? chip;

  /// Names the code blocks across rebuilds, so a growing one is coloured from where it
  /// left off.
  final String codeKey;

  /// Blocks take the full width (a reply); otherwise the column is as wide as its widest
  /// block, so a bubble around it fits its text.
  final bool fill;

  const MarkdownBlocks({
    super.key,
    required this.blocks,
    this.size = 15,
    this.color,
    this.onLink,
    this.onFile,
    this.chip,
    this.codeKey = '',
    this.fill = true,
  });

  @override
  State<MarkdownBlocks> createState() => _MarkdownBlocksState();
}

class _MarkdownBlocksState extends State<MarkdownBlocks> {
  /// Built blocks by position, with the block each was built from.
  final _built = <int, (Object?, Widget)>{};
  Object? _builtFor;

  void _link(String href) => widget.onLink?.call(href);
  void _file(String path) => widget.onFile?.call(path);
  Widget? _chip(int index) => widget.chip?.call(index);

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    // What every block's look depends on besides the block itself.
    final look = (
      c,
      widget.size,
      widget.color,
      widget.codeKey,
      widget.chip == null,
      widget.fill,
    );
    if (look != _builtFor) {
      _built.clear();
      _builtFor = look;
    }
    final blocks = widget.blocks;
    _built.removeWhere((i, _) => i >= blocks.length);
    final children = <Widget>[];
    final style = _Style.of(context, widget.size, widget.color, widget.fill);
    for (final (i, block) in blocks.indexed) {
      final built = _built[i];
      if (built != null && identical(built.$1, block)) {
        children.add(built.$2);
        continue;
      }
      final child = _Block(
        key: ValueKey(i),
        first: i == 0,
        block: block as Map,
        style: style,
        link: _link,
        file: _file,
        chip: _chip,
        codeKey: '${widget.codeKey}#$i',
      );
      _built[i] = (block, child);
      children.add(child);
    }
    return Column(
      crossAxisAlignment: widget.fill
          ? CrossAxisAlignment.stretch
          : CrossAxisAlignment.start,
      mainAxisSize: MainAxisSize.min,
      children: children,
    );
  }
}

/// What blocks are drawn with.
class _Style {
  final TextStyle text, code;
  final HubColors colors;
  final double size;

  /// Blocks take the full width; otherwise lists and notes fit their text.
  final bool fill;
  const _Style(this.text, this.code, this.colors, this.size, this.fill);

  /// A column's children at full width, or at their own.
  CrossAxisAlignment get across =>
      fill ? CrossAxisAlignment.stretch : CrossAxisAlignment.start;

  /// What follows a list marker or note label: the rest of the row, or as much of
  /// it as it needs.
  Widget rest(Widget child) =>
      fill ? Expanded(child: child) : Flexible(child: child);

  factory _Style.of(
    BuildContext context,
    double size,
    Color? color,
    bool fill,
  ) {
    final c = HubColors.of(context);
    final ink = color ?? c.text;
    return _Style(
      TextStyle(fontSize: size, height: 1.65, color: ink),
      codeStyle.copyWith(
        fontSize: size - 1.5,
        backgroundColor: c.code,
        color: ink,
      ),
      c,
      size,
      fill,
    );
  }
}

class _Block extends StatelessWidget {
  /// Space goes between blocks, none around them, so a bubble's padding is even.
  final bool first;
  final Map block;
  final _Style style;
  final ValueChanged<String> link, file;
  final Widget? Function(int) chip;
  final String codeKey;
  const _Block({
    super.key,
    required this.first,
    required this.block,
    required this.style,
    required this.link,
    required this.file,
    required this.chip,
    required this.codeKey,
  });

  @override
  Widget build(BuildContext context) => Padding(
    padding: EdgeInsets.only(top: first ? 0 : _gap),
    child: _content(context, block),
  );

  Widget _children(BuildContext context, List blocks, String key) => Column(
    crossAxisAlignment: style.across,
    mainAxisSize: MainAxisSize.min,
    children: [
      for (final (i, b) in blocks.indexed)
        Padding(
          padding: EdgeInsets.only(top: i == 0 ? 0 : _gap),
          child: _content(context, b as Map, key: '$key.$i'),
        ),
    ],
  );

  Widget _paragraph(List inlines, TextStyle text) => _Paragraph(
    inlines: inlines,
    style: text,
    code: style.code,
    link: link,
    file: file,
    chip: chip,
  );

  Widget _content(BuildContext context, Map block, {String? key}) {
    final c = style.colors;
    final text = style.text;
    switch (block['t']) {
      case 'paragraph':
        return _paragraph(block['inlines'] as List, text);
      case 'heading':
        final level = (block['level'] as num).toInt();
        final scale = switch (level) {
          1 => 1.5,
          2 => 1.3,
          3 => 1.15,
          _ => 1.0,
        };
        return Padding(
          padding: EdgeInsets.only(top: level <= 2 ? 6 : 2),
          child: _paragraph(
            block['inlines'] as List,
            text.copyWith(
              fontSize: style.size * scale,
              height: 1.4,
              fontWeight: FontWeight.w600,
            ),
          ),
        );
      case 'code':
        return CodeBlock(
          code: block['text'] as String,
          info: block['info'] as String,
          color: text.color!,
          highlightKey: key ?? codeKey,
        );
      case 'math':
        return _DisplayedMath(
          tex: block['tex'] as String,
          source: block['source'] as String,
          style: text,
          fallback: style.code,
        );
      case 'quote':
        return Container(
          padding: const EdgeInsets.only(left: 12),
          decoration: BoxDecoration(
            border: Border(left: BorderSide(color: c.border, width: 3)),
          ),
          child: _children(context, block['blocks'] as List, key ?? codeKey),
        );
      case 'list':
        return _list(context, block, key ?? codeKey);
      case 'table':
        return _table(block);
      case 'rule':
        return Divider(color: c.border, height: 16);
      case 'note':
        return DefaultTextStyle.merge(
          style: TextStyle(fontSize: style.size - 2, color: c.muted),
          child: Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            mainAxisSize: style.fill ? MainAxisSize.max : MainAxisSize.min,
            children: [
              Text(
                '${block['label']}. ',
                style: text.copyWith(color: c.subtle),
              ),
              style.rest(
                _children(context, block['blocks'] as List, key ?? codeKey),
              ),
            ],
          ),
        );
      default:
        // HTML and anything newer show as written.
        return Text('${block['text'] ?? ''}', style: text);
    }
  }

  Widget _list(BuildContext context, Map block, String key) {
    final c = style.colors;
    final start = (block['start'] as num?)?.toInt();
    final tight = block['tight'] == true;
    final items = block['items'] as List;
    final text = style.text;
    return Column(
      crossAxisAlignment: style.across,
      mainAxisSize: MainAxisSize.min,
      children: [
        for (final (i, item) in items.indexed)
          Padding(
            padding: EdgeInsets.only(top: i == 0 ? 0 : (tight ? 2 : _gap)),
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.start,
              mainAxisSize: style.fill ? MainAxisSize.max : MainAxisSize.min,
              children: [
                SizedBox(
                  width: start == null ? 18 : 26,
                  child: switch (item['task']) {
                    final bool done => Padding(
                      padding: EdgeInsets.only(top: style.size * .32),
                      child: Icon(
                        done ? Icons.check_box : Icons.check_box_outline_blank,
                        size: style.size + 1,
                        color: done ? c.accent : c.subtle,
                      ),
                    ),
                    _ => Text(
                      start == null ? '•' : '${start + i}.',
                      style: text.copyWith(color: c.muted),
                    ),
                  },
                ),
                style.rest(
                  Column(
                    crossAxisAlignment: style.across,
                    mainAxisSize: MainAxisSize.min,
                    children: [
                      for (final (j, b) in (item['blocks'] as List).indexed)
                        Padding(
                          padding: EdgeInsets.only(
                            top: j == 0 ? 0 : (tight ? 2 : _gap),
                          ),
                          child: _content(context, b as Map, key: '$key.$i.$j'),
                        ),
                    ],
                  ),
                ),
              ],
            ),
          ),
      ],
    );
  }

  Widget _table(Map block) {
    final c = style.colors;
    final align = (block['align'] as List).cast<String>();
    final head = block['head'] as List;
    final rows = block['rows'] as List;
    TextAlign alignOf(int i) => switch (align.elementAtOrNull(i)) {
      'center' => TextAlign.center,
      'right' => TextAlign.right,
      _ => TextAlign.left,
    };
    TableRow row(List cells, {bool header = false}) => TableRow(
      decoration: header ? BoxDecoration(color: c.code) : null,
      children: [
        for (var i = 0; i < head.length; i++)
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 6),
            child: _Paragraph(
              inlines: (cells.elementAtOrNull(i) as List?) ?? const [],
              style: style.text.copyWith(
                fontWeight: header ? FontWeight.w600 : null,
                height: 1.45,
              ),
              code: style.code,
              link: link,
              file: file,
              chip: chip,
              align: alignOf(i),
            ),
          ),
      ],
    );
    // Columns share the width: formulas cannot report the intrinsic sizes a table would
    // need to size columns by content.
    return Table(
      defaultColumnWidth: const FlexColumnWidth(),
      border: TableBorder.all(color: c.border),
      children: [row(head, header: true), for (final r in rows) row(r as List)],
    );
  }
}

/// Inline content: styled text, links, code, formulas, citation chips and file links. Holds
/// the recognizers its links tap through.
class _Paragraph extends StatefulWidget {
  final List inlines;
  final TextStyle style, code;
  final ValueChanged<String> link, file;
  final Widget? Function(int) chip;
  final TextAlign align;
  const _Paragraph({
    required this.inlines,
    required this.style,
    required this.code,
    required this.link,
    required this.file,
    required this.chip,
    this.align = TextAlign.start,
  });

  @override
  State<_Paragraph> createState() => _ParagraphState();
}

class _ParagraphState extends State<_Paragraph> {
  final _recognizers = <TapGestureRecognizer>[];

  void _release() {
    for (final r in _recognizers) {
      r.dispose();
    }
    _recognizers.clear();
  }

  @override
  void dispose() {
    _release();
    super.dispose();
  }

  TapGestureRecognizer _tap(String href) {
    final r = TapGestureRecognizer()..onTap = () => widget.link(href);
    _recognizers.add(r);
    return r;
  }

  @override
  Widget build(BuildContext context) {
    _release();
    final c = HubColors.of(context);
    final base = widget.style;
    final spans = <InlineSpan>[];
    for (final inline in widget.inlines) {
      final href = inline['href'] as String?;
      switch (inline['t']) {
        case 'text':
          final style = (inline['style'] as num?)?.toInt() ?? 0;
          spans.add(
            TextSpan(
              text: inline['text'] as String,
              style: TextStyle(
                fontWeight: style & 1 != 0 ? FontWeight.w600 : null,
                fontStyle: style & 2 != 0 ? FontStyle.italic : null,
                decoration: TextDecoration.combine([
                  if (style & 4 != 0) TextDecoration.lineThrough,
                  if (href != null) TextDecoration.underline,
                ]),
                decorationColor: href != null
                    ? c.accent.withValues(alpha: .4)
                    : null,
                color: href != null ? c.accent : null,
              ),
              recognizer: href == null ? null : _tap(href),
              mouseCursor: href == null ? null : SystemMouseCursors.click,
            ),
          );
        case 'code':
          spans.add(
            TextSpan(
              text: inline['text'] as String,
              style: widget.code.copyWith(
                color: href != null ? c.accent : null,
              ),
              recognizer: href == null ? null : _tap(href),
            ),
          );
        case 'math':
          spans.addAll(
            _math(
              inline['tex'] as String,
              inline['source'] as String,
              inline['display'] == true,
            ),
          );
        case 'chip':
          final chip = widget.chip((inline['index'] as num).toInt());
          if (chip != null) {
            spans.add(
              WidgetSpan(alignment: PlaceholderAlignment.middle, child: chip),
            );
          }
        case 'file':
          spans.add(_file(inline['label'] as String, inline['path'] as String));
        case 'image':
          final alt = '${inline['alt']}'.trim();
          final url = inline['url'] as String;
          spans.add(
            TextSpan(
              text: '🖼 ${alt.isEmpty ? url : alt}',
              style: TextStyle(color: c.accent),
              recognizer: _tap(url),
              mouseCursor: SystemMouseCursors.click,
            ),
          );
        case 'break':
          spans.add(const TextSpan(text: '\n'));
      }
    }
    return Text.rich(
      TextSpan(style: base, children: spans),
      textAlign: widget.align,
    );
  }

  List<InlineSpan> _math(String tex, String source, bool display) {
    final math = Math.tex(
      tex,
      mathStyle: display ? MathStyle.display : MathStyle.text,
      // KaTeX's size: its x-height is smaller than the text's.
      textStyle: TextStyle(
        fontSize: (widget.style.fontSize ?? 15) * 1.21,
        color: widget.style.color,
      ),
      onErrorFallback: (_) => Text(source, style: widget.code),
    );
    if (display) {
      return [
        WidgetSpan(
          child: SizedBox(
            width: double.infinity,
            child: Padding(
              padding: const EdgeInsets.symmetric(vertical: 6),
              child: Center(
                child: SingleChildScrollView(
                  scrollDirection: Axis.horizontal,
                  child: math,
                ),
              ),
            ),
          ),
        ),
      ];
    }
    // An inline formula wraps where TeX would break it.
    return [
      for (final part in math.texBreak().parts)
        WidgetSpan(
          alignment: PlaceholderAlignment.baseline,
          baseline: TextBaseline.alphabetic,
          child: part,
        ),
    ];
  }

  /// A link to a sandbox file: the file's glyph before the label, opened on tap.
  InlineSpan _file(String label, String path) {
    final c = HubColors.of(context);
    final style = widget.style.copyWith(color: c.accent);
    return WidgetSpan(
      alignment: PlaceholderAlignment.middle,
      child: Semantics(
        link: true,
        child: InkWell(
          borderRadius: BorderRadius.circular(4),
          onTap: () => widget.file(path),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              Icon(
                Icons.insert_drive_file_outlined,
                size: (style.fontSize ?? 15) * .95,
                color: c.accent,
              ),
              const SizedBox(width: 2),
              Text(label, style: style),
            ],
          ),
        ),
      ),
    );
  }
}

/// A formula on lines of its own: centred, and scrolling when wider than the text.
class _DisplayedMath extends StatelessWidget {
  final String tex, source;
  final TextStyle style, fallback;
  const _DisplayedMath({
    required this.tex,
    required this.source,
    required this.style,
    required this.fallback,
  });

  @override
  Widget build(BuildContext context) => Padding(
    padding: const EdgeInsets.symmetric(vertical: 6),
    child: Center(
      child: SingleChildScrollView(
        scrollDirection: Axis.horizontal,
        child: Math.tex(
          tex,
          mathStyle: MathStyle.display,
          textStyle: TextStyle(
            fontSize: (style.fontSize ?? 15) * 1.21,
            color: style.color,
          ),
          onErrorFallback: (_) => Text(source, style: fallback),
        ),
      ),
    ),
  );
}

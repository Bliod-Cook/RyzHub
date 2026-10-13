import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:http/http.dart' as http;

import '../l10n.dart';
import '../rust/api.dart' as rust;
import '../theme.dart';
import 'unfold.dart';

/// Atom One's token colours, by the highlight.js class the kernel gives a token.
const _atomOne = {
  Brightness.light: {
    'comment': TextStyle(color: Color(0xffa0a1a7), fontStyle: FontStyle.italic),
    'quote': TextStyle(color: Color(0xffa0a1a7), fontStyle: FontStyle.italic),
    'keyword': TextStyle(color: Color(0xffa626a4)),
    'section': TextStyle(color: Color(0xffe45649)),
    'name': TextStyle(color: Color(0xffe45649)),
    'deletion': TextStyle(color: Color(0xffe45649)),
    'subst': TextStyle(color: Color(0xffe45649)),
    'literal': TextStyle(color: Color(0xff0184bb)),
    'string': TextStyle(color: Color(0xff50a14f)),
    'regexp': TextStyle(color: Color(0xff50a14f)),
    'addition': TextStyle(color: Color(0xff50a14f)),
    'attr': TextStyle(color: Color(0xff986801)),
    'variable': TextStyle(color: Color(0xff986801)),
    'type': TextStyle(color: Color(0xff986801)),
    'number': TextStyle(color: Color(0xff986801)),
    'symbol': TextStyle(color: Color(0xff4078f2)),
    'bullet': TextStyle(color: Color(0xff4078f2)),
    'link': TextStyle(color: Color(0xff4078f2)),
    'meta': TextStyle(color: Color(0xff4078f2)),
    'title': TextStyle(color: Color(0xff4078f2)),
    'built_in': TextStyle(color: Color(0xffc18401)),
    'class': TextStyle(color: Color(0xffc18401)),
    'emphasis': TextStyle(fontStyle: FontStyle.italic),
    'strong': TextStyle(fontWeight: FontWeight.bold),
  },
  Brightness.dark: {
    'comment': TextStyle(color: Color(0xff5c6370), fontStyle: FontStyle.italic),
    'quote': TextStyle(color: Color(0xff5c6370), fontStyle: FontStyle.italic),
    'keyword': TextStyle(color: Color(0xffc678dd)),
    'section': TextStyle(color: Color(0xffe06c75)),
    'name': TextStyle(color: Color(0xffe06c75)),
    'deletion': TextStyle(color: Color(0xffe06c75)),
    'subst': TextStyle(color: Color(0xffe06c75)),
    'literal': TextStyle(color: Color(0xff56b6c2)),
    'string': TextStyle(color: Color(0xff98c379)),
    'regexp': TextStyle(color: Color(0xff98c379)),
    'addition': TextStyle(color: Color(0xff98c379)),
    'attr': TextStyle(color: Color(0xffd19a66)),
    'variable': TextStyle(color: Color(0xffd19a66)),
    'type': TextStyle(color: Color(0xffd19a66)),
    'number': TextStyle(color: Color(0xffd19a66)),
    'symbol': TextStyle(color: Color(0xff61aeee)),
    'bullet': TextStyle(color: Color(0xff61aeee)),
    'link': TextStyle(color: Color(0xff61aeee)),
    'meta': TextStyle(color: Color(0xff61aeee)),
    'title': TextStyle(color: Color(0xff61aeee)),
    'built_in': TextStyle(color: Color(0xffe6c07b)),
    'class': TextStyle(color: Color(0xffe6c07b)),
    'emphasis': TextStyle(fontStyle: FontStyle.italic),
    'strong': TextStyle(fontWeight: FontWeight.bold),
  },
};

/// The grammars code is coloured with have loaded. The web loads them on first use: code shows
/// plain until then, and coloured once they arrive.
final syntaxesLoaded = ValueNotifier<bool>(false);
Future<void>? _loading;

bool _syntaxesReady() {
  if (syntaxesLoaded.value) return true;
  if (rust.syntaxesReady()) return true;
  _loading ??= () async {
    try {
      final response = await http.get(Uri.base.resolve('syntaxes.bin'));
      if (response.statusCode != 200) return;
      rust.loadSyntaxes(dump: response.bodyBytes);
      syntaxesLoaded.value = true;
    } catch (_) {
      // Code stays plain; the next page load tries again.
    }
  }();
  return false;
}

/// Coloured code, most recently used last: each rebuild of an unchanged block finds its spans.
final _spans = <String, (String, String, List)>{};

/// [code]'s spans as `[text, class]`, or null to show it plain.
List? _highlighted(String key, String language, String code) {
  final hit = _spans.remove(key);
  if (hit != null && hit.$1 == language && hit.$2 == code) {
    return (_spans[key] = hit).$3;
  }
  if (!_syntaxesReady()) return null;
  final json = rust.highlight(key: key, language: language, code: code);
  if (json == null) return null;
  final spans = jsonDecode(json) as List;
  _spans[key] = (language, code, spans);
  if (_spans.length > 64) _spans.remove(_spans.keys.first);
  return spans;
}

/// Whether a fence's info string names a diff.
bool _isDiff(String? info) {
  final word = (info ?? '').trim().split(RegExp(r'[\s,{]')).first.toLowerCase();
  return word == 'diff' || word == 'patch';
}

/// Selectable source code in [style], coloured by syntax when [language] names a known one.
/// Every line is as tall as the style's, whatever font a glyph falls back to, so lines stay
/// level with numbers beside them.
class CodeText extends StatelessWidget {
  final String code;
  final String? language;
  final TextStyle style;

  /// Names the code across rebuilds, so code that grows is coloured from where it left off.
  final String? highlightKey;
  const CodeText(
    this.code, {
    super.key,
    this.language,
    required this.style,
    this.highlightKey,
  });

  @override
  Widget build(BuildContext context) {
    final language = this.language;
    if (language == null || language.trim().isEmpty) return _plain();
    if (_isDiff(language)) return DiffText(code, style: style);
    return ValueListenableBuilder(
      valueListenable: syntaxesLoaded,
      builder: (context, _, _) {
        final spans = _highlighted(
          highlightKey ?? '$language:${code.hashCode}:${code.length}',
          language,
          code,
        );
        if (spans == null) return _plain();
        final theme = _atomOne[Theme.of(context).brightness]!;
        return SelectableText.rich(
          TextSpan(
            style: style,
            children: [
              for (final span in spans)
                TextSpan(text: span[0] as String, style: theme[span[1]]),
            ],
          ),
          strutStyle: StrutStyle.fromTextStyle(style, forceStrutHeight: true),
        );
      },
    );
  }

  Widget _plain() => SelectableText(
    code,
    style: style,
    strutStyle: StrutStyle.fromTextStyle(style, forceStrutHeight: true),
  );
}

/// A unified diff: added and removed lines on tinted ground, hunk headers as headings.
class DiffText extends StatelessWidget {
  final String diff;
  final TextStyle style;
  const DiffText(this.diff, {super.key, required this.style});

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final lines = jsonDecode(rust.diffLines(text: diff)) as List;
    final added = c.success.withValues(alpha: .16);
    final removed = c.danger.withValues(alpha: .14);
    return SelectableText.rich(
      TextSpan(
        style: style,
        children: [
          for (final (i, line) in lines.indexed)
            TextSpan(
              text:
                  '${switch (line['kind']) {
                    'add' => '+',
                    'del' => '-',
                    'ctx' => ' ',
                    _ => '',
                  }}${line['text']}${i == lines.length - 1 ? '' : '\n'}',
              style: switch (line['kind']) {
                'add' => TextStyle(backgroundColor: added),
                'del' => TextStyle(backgroundColor: removed),
                'hunk' => TextStyle(color: c.accent),
                'meta' => TextStyle(color: c.subtle),
                _ => null,
              },
            ),
        ],
      ),
      strutStyle: StrutStyle.fromTextStyle(style, forceStrutHeight: true),
    );
  }
}

/// The 16 colours terminals start with, then the 6×6×6 cube and the greys of the 256.
Color _palette(int index) {
  const base = [
    Color(0xff000000),
    Color(0xffcd3131),
    Color(0xff0dbc79),
    Color(0xffe5e510),
    Color(0xff2472c8),
    Color(0xffbc3fbc),
    Color(0xff11a8cd),
    Color(0xffe5e5e5),
    Color(0xff666666),
    Color(0xfff14c4c),
    Color(0xff23d18b),
    Color(0xfff5f543),
    Color(0xff3b8eea),
    Color(0xffd670d6),
    Color(0xff29b8db),
    Color(0xffffffff),
  ];
  if (index < 16) return base[index];
  if (index < 232) {
    final i = index - 16;
    int level(int v) => v == 0 ? 0 : 55 + v * 40;
    return Color.fromARGB(255, level(i ~/ 36), level(i ~/ 6 % 6), level(i % 6));
  }
  final grey = 8 + (index - 232) * 10;
  return Color.fromARGB(255, grey, grey, grey);
}

Color? _color(Object? value) => switch (value) {
  final int index => _palette(index),
  final String hex when hex.length == 7 => Color(
    0xff000000 | int.parse(hex.substring(1), radix: 16),
  ),
  _ => null,
};

/// Terminal output with its colours and styles; other control sequences are dropped.
class AnsiText extends StatelessWidget {
  final String text;
  final TextStyle style;
  const AnsiText(this.text, {super.key, required this.style});

  /// Whether [text] holds terminal escapes.
  static bool styled(String text) => text.contains('\x1b');

  @override
  Widget build(BuildContext context) {
    final spans = jsonDecode(rust.ansiSpans(text: text)) as List;
    return SelectableText.rich(
      TextSpan(
        style: style,
        children: [
          for (final span in spans)
            () {
              final flags = (span['style'] as num?)?.toInt() ?? 0;
              var fg = _color(span['fg']);
              var bg = _color(span['bg']);
              if (flags & 16 != 0) {
                (fg, bg) = (bg ?? style.backgroundColor, fg ?? style.color);
              }
              return TextSpan(
                text: span['text'] as String,
                style: TextStyle(
                  color: flags & 2 != 0
                      ? (fg ?? style.color)?.withValues(alpha: .6)
                      : fg,
                  backgroundColor: bg,
                  fontWeight: flags & 1 != 0 ? FontWeight.bold : null,
                  fontStyle: flags & 4 != 0 ? FontStyle.italic : null,
                  decoration: TextDecoration.combine([
                    if (flags & 8 != 0) TextDecoration.underline,
                    if (flags & 32 != 0) TextDecoration.lineThrough,
                  ]),
                ),
              );
            }(),
        ],
      ),
      strutStyle: StrutStyle.fromTextStyle(style, forceStrutHeight: true),
    );
  }
}

/// Code blocks longer than this open folded.
const _foldLines = 20;

/// The lines a folded code block shows.
const _peekLines = 12;

/// A fenced code block: its info string, a copy button, numbered lines that
/// scroll sideways rather than wrap, and a fold when it runs long.
class CodeBlock extends StatefulWidget {
  /// As fenced, up to its closing fence.
  final String code;
  final String info;
  final Color color;

  /// Names the block across rebuilds, so code that grows is coloured from where it left off.
  final String? highlightKey;
  const CodeBlock({
    super.key,
    required this.code,
    required this.info,
    required this.color,
    this.highlightKey,
  });

  @override
  State<CodeBlock> createState() => _CodeBlockState();
}

class _CodeBlockState extends State<CodeBlock> {
  /// The code without the line break before its closing fence.
  String get text => widget.code.endsWith('\n')
      ? widget.code.substring(0, widget.code.length - 1)
      : widget.code;

  int get lines => '\n'.allMatches(text).length + 1;

  /// Long code opens folded. Code that grows long as a reply streams stays
  /// open under the reader's eyes.
  late bool folded = lines > _foldLines;

  void toggle() => setState(() => folded = !folded);

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final style = TextStyle(
      fontFamily: 'monospace',
      fontFamilyFallback: const ['RyzHub Sans'],
      fontSize: 13.5,
      height: 1.5,
      color: widget.color,
    );
    final lineHeight =
        MediaQuery.textScalerOf(context).scale(style.fontSize!) * style.height!;
    final lines = this.lines;
    final foldable = lines > _foldLines;
    return Container(
      margin: const EdgeInsets.symmetric(vertical: Space.s10),
      decoration: BoxDecoration(
        color: c.code,
        borderRadius: BorderRadius.circular(14),
        border: Border.all(color: c.border),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(
              Space.s14,
              Space.s2,
              Space.s4,
              0,
            ),
            child: Row(
              children: [
                Expanded(
                  child: Text(
                    widget.info,
                    style: TextStyle(fontSize: 12, color: c.subtle),
                  ),
                ),
                if (foldable)
                  IconButton(
                    tooltip: folded
                        ? tr('展开代码', 'Expand code')
                        : tr('折叠代码', 'Collapse code'),
                    icon: Icon(
                      folded ? Icons.unfold_more : Icons.unfold_less,
                      size: 16,
                    ),
                    onPressed: toggle,
                  ),
                IconButton(
                  tooltip: tr('复制代码', 'Copy code'),
                  icon: const Icon(Icons.copy_rounded, size: 15),
                  onPressed: () =>
                      Clipboard.setData(ClipboardData(text: widget.code)),
                ),
              ],
            ),
          ),
          Stack(
            children: [
              Unfold(
                open: !folded,
                peek: foldable ? _peekLines * lineHeight : 0,
                builder: (context) => Padding(
                  padding: const EdgeInsets.only(left: Space.s14),
                  child: Row(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Text(
                        [for (var i = 1; i <= lines; i++) i].join('\n'),
                        textAlign: TextAlign.right,
                        style: style.copyWith(color: c.subtle),
                        strutStyle: StrutStyle.fromTextStyle(
                          style,
                          forceStrutHeight: true,
                        ),
                      ),
                      const SizedBox(width: 16),
                      Expanded(
                        child: SingleChildScrollView(
                          scrollDirection: Axis.horizontal,
                          padding: const EdgeInsets.fromLTRB(
                            0,
                            0,
                            Space.s14,
                            Space.s14,
                          ),
                          child: CodeText(
                            text,
                            language: widget.info,
                            style: style,
                            highlightKey: widget.highlightKey,
                          ),
                        ),
                      ),
                    ],
                  ),
                ),
              ),
              // Folded code fades out into the button that unfolds it.
              Positioned(
                left: 0,
                right: 0,
                bottom: 0,
                height: lineHeight * 2,
                child: IgnorePointer(
                  child: AnimatedOpacity(
                    opacity: folded ? 1 : 0,
                    duration: unfoldDuration,
                    child: DecoratedBox(
                      decoration: BoxDecoration(
                        gradient: LinearGradient(
                          begin: Alignment.topCenter,
                          end: Alignment.bottomCenter,
                          colors: [c.code.withValues(alpha: 0), c.code],
                        ),
                      ),
                    ),
                  ),
                ),
              ),
            ],
          ),
          // Folds with the code, so what is above stays put as both change.
          if (foldable)
            Unfold(
              open: folded,
              builder: (context) => TextButton(
                onPressed: toggle,
                style: TextButton.styleFrom(
                  foregroundColor: c.muted,
                  minimumSize: const Size.fromHeight(38),
                  textStyle: const TextStyle(fontSize: 12.5),
                  shape: const RoundedRectangleBorder(
                    borderRadius: BorderRadius.vertical(
                      bottom: Radius.circular(13),
                    ),
                  ),
                ),
                child: Row(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Text(tr('展开全部 · 共 $lines 行', 'Show all $lines lines')),
                    const SizedBox(width: 4),
                    const Icon(Icons.expand_more, size: 16),
                  ],
                ),
              ),
            ),
        ],
      ),
    );
  }
}

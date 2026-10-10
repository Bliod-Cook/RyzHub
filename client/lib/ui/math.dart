import 'package:flutter/material.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';
import 'package:flutter_math_fork/flutter_math.dart';
import 'package:markdown/markdown.dart' as md;

/// A formula for [MathBuilder]: its TeX, and its [source] as written, shown
/// when the TeX does not parse.
md.Element _formula(String tex, String source, {required bool display}) =>
    md.Element.text('math', tex)
      ..attributes['source'] = source
      ..attributes['display'] = '$display';

/// TeX in a paragraph: `$$…$$` and `\[…\]` displayed, `\(…\)` and `$…$`
/// inline. Tried before Markdown's own syntaxes, so `\(` keeps its backslash
/// and `_` stays a subscript. A `$` pair follows Pandoc so prices stay text:
/// no space inside either `$`, no digit after the closing one, on one line.
class MathSyntax extends md.InlineSyntax {
  MathSyntax()
    : super(
        r'\$\$([\s\S]+?)\$\$|\\\[([\s\S]+?)\\\]|\\\(([\s\S]+?)\\\)'
        r'|\$(?![\s$])((?:\\.|[^\\$\n])+?)(?<!\s)\$(?!\d)',
      );

  @override
  bool onMatch(md.InlineParser parser, Match match) {
    final display = match[1] ?? match[2];
    parser.addNode(
      _formula(
        display ?? match[3] ?? match[4]!,
        match[0]!,
        display: display != null,
      ),
    );
    return true;
  }
}

/// A displayed formula on lines of its own, opened by `$$` or `\[` at a
/// line's start and closed at a line's end, so none of its lines starts a
/// list or ends a paragraph. It is a paragraph of the one formula: a block
/// builder would add its tag to flutter_markdown_plus's block tags on every
/// build.
class MathBlockSyntax extends md.BlockSyntax {
  const MathBlockSyntax();

  static final _opening = RegExp(r'^ {0,3}(\$\$|\\\[)');

  @override
  RegExp get pattern => _opening;

  @override
  bool canParse(md.BlockParser parser) => _read(parser) != null;

  @override
  md.Node parse(md.BlockParser parser) {
    final (lines, source, tex) = _read(parser)!;
    for (var i = 0; i < lines; i++) {
      parser.advance();
    }
    return md.Element('p', [_formula(tex, source, display: true)]);
  }

  /// The formula opened at the current line: how many lines it takes, its
  /// source and its TeX. Null when it is not closed at the end of a line.
  (int, String, String)? _read(md.BlockParser parser) {
    final open = _opening.firstMatch(parser.current.content);
    if (open == null) return null;
    final close = open[1] == r'$$' ? r'$$' : r'\]';
    final lines = <String>[];
    for (var i = 0; ; i++) {
      final line = i == 0 ? parser.current : parser.peek(i);
      if (line == null) return null;
      lines.add(line.content);
      final at = line.content.indexOf(close, i == 0 ? open.end : 0);
      if (at < 0) continue;
      if (line.content.substring(at + close.length).trim().isNotEmpty) {
        return null;
      }
      final source = lines.join('\n').trim();
      final tex = source.substring(
        open[1]!.length,
        source.length - close.length,
      );
      return tex.trim().isEmpty ? null : (lines.length, source, tex);
    }
  }
}

/// Renders a formula in the paragraph's size and colour. An inline one wraps
/// where TeX would break it; a displayed one takes a line of its own, centred,
/// and scrolls when wider than the text.
class MathBuilder extends MarkdownElementBuilder {
  /// The paragraph text, and the code style a formula's source shows in when
  /// its TeX does not parse.
  final TextStyle style, fallback;
  MathBuilder({required this.style, required this.fallback});

  @override
  Widget? visitElementAfterWithContext(
    BuildContext context,
    md.Element element,
    TextStyle? preferredStyle,
    TextStyle? parentStyle,
  ) {
    final text = style.merge(parentStyle);
    final display = element.attributes['display'] == 'true';
    final math = Math.tex(
      element.textContent,
      mathStyle: display ? MathStyle.display : MathStyle.text,
      // KaTeX's size: its x-height is smaller than the text's.
      textStyle: TextStyle(
        fontSize: (text.fontSize ?? 15) * 1.21,
        color: text.color,
      ),
      onErrorFallback: (_) =>
          Text(element.attributes['source'] ?? '', style: fallback),
    );
    if (display) {
      return Text.rich(
        TextSpan(
          children: [
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
          ],
        ),
      );
    }
    return Text.rich(
      TextSpan(
        children: [
          for (final part in math.texBreak().parts)
            WidgetSpan(
              alignment: PlaceholderAlignment.baseline,
              baseline: TextBaseline.alphabetic,
              child: part,
            ),
        ],
      ),
    );
  }
}

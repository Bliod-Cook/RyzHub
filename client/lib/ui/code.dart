import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:re_highlight/languages/bash.dart';
import 'package:re_highlight/languages/c.dart';
import 'package:re_highlight/languages/cmake.dart';
import 'package:re_highlight/languages/cpp.dart';
import 'package:re_highlight/languages/csharp.dart';
import 'package:re_highlight/languages/css.dart';
import 'package:re_highlight/languages/dart.dart';
import 'package:re_highlight/languages/diff.dart';
import 'package:re_highlight/languages/dockerfile.dart';
import 'package:re_highlight/languages/dos.dart';
import 'package:re_highlight/languages/go.dart';
import 'package:re_highlight/languages/graphql.dart';
import 'package:re_highlight/languages/ini.dart';
import 'package:re_highlight/languages/java.dart';
import 'package:re_highlight/languages/javascript.dart';
import 'package:re_highlight/languages/json.dart';
import 'package:re_highlight/languages/kotlin.dart';
import 'package:re_highlight/languages/latex.dart';
import 'package:re_highlight/languages/less.dart';
import 'package:re_highlight/languages/lua.dart';
import 'package:re_highlight/languages/makefile.dart';
import 'package:re_highlight/languages/markdown.dart';
import 'package:re_highlight/languages/nginx.dart';
import 'package:re_highlight/languages/objectivec.dart';
import 'package:re_highlight/languages/perl.dart';
import 'package:re_highlight/languages/php.dart';
import 'package:re_highlight/languages/powershell.dart';
import 'package:re_highlight/languages/protobuf.dart';
import 'package:re_highlight/languages/python.dart';
import 'package:re_highlight/languages/r.dart';
import 'package:re_highlight/languages/ruby.dart';
import 'package:re_highlight/languages/rust.dart';
import 'package:re_highlight/languages/scala.dart';
import 'package:re_highlight/languages/scss.dart';
import 'package:re_highlight/languages/shell.dart';
import 'package:re_highlight/languages/sql.dart';
import 'package:re_highlight/languages/swift.dart';
import 'package:re_highlight/languages/typescript.dart';
import 'package:re_highlight/languages/vue.dart';
import 'package:re_highlight/languages/xml.dart';
import 'package:re_highlight/languages/yaml.dart';
import 'package:re_highlight/re_highlight.dart';
import 'package:re_highlight/styles/atom-one-dark.dart';
import 'package:re_highlight/styles/atom-one-light.dart';

import '../l10n.dart';
import '../theme.dart';
import 'unfold.dart';

/// Languages code is coloured in: a subset of highlight.js's keeps the web
/// bundle small. Each registers its own aliases (rs, py, ts, yml, html, …).
final _highlight = Highlight()
  ..registerLanguages({
    'bash': langBash,
    'c': langC,
    'cmake': langCmake,
    'cpp': langCpp,
    'csharp': langCsharp,
    'css': langCss,
    'dart': langDart,
    'diff': langDiff,
    'dockerfile': langDockerfile,
    'dos': langDos,
    'go': langGo,
    'graphql': langGraphql,
    'ini': langIni,
    'java': langJava,
    'javascript': langJavascript,
    'json': langJson,
    'kotlin': langKotlin,
    'latex': langLatex,
    'less': langLess,
    'lua': langLua,
    'makefile': langMakefile,
    'markdown': langMarkdown,
    'nginx': langNginx,
    'objectivec': langObjectivec,
    'perl': langPerl,
    'php': langPhp,
    'powershell': langPowershell,
    'protobuf': langProtobuf,
    'python': langPython,
    'r': langR,
    'ruby': langRuby,
    'rust': langRust,
    'scala': langScala,
    'scss': langScss,
    'shell': langShell,
    'sql': langSql,
    'swift': langSwift,
    'typescript': langTypescript,
    'vue': langVue,
    'xml': langXml,
    'yaml': langYaml,
  });

/// Names highlight.js has no alias for: file extensions, and fence labels
/// models write. Its `shell` is a console session with prompts; a ```shell
/// fence holds a plain script.
const _aliases = {
  'shell': 'bash',
  'zsh': 'bash',
  'fish': 'bash',
  'jsonl': 'json',
  'jsonc': 'json',
  'json5': 'json',
  'htm': 'xml',
  'svelte': 'xml',
  'm': 'objectivec',
  'conf': 'ini',
  'cfg': 'ini',
  'env': 'ini',
  'node': 'javascript',
  'postgres': 'sql',
  'postgresql': 'sql',
  'mysql': 'sql',
  'sqlite': 'sql',
};

/// Atom One's token colours. Its root colours give way to the surrounding
/// card and text.
final _themes = {
  Brightness.light: {...atomOneLightTheme}..remove('root'),
  Brightness.dark: {...atomOneDarkTheme}..remove('root'),
};

/// Longer code shows plain: highlighting runs on the UI thread, and the web
/// has no other.
const _maxHighlighted = 100000;

/// Highlighted code, most recently used last. Every streamed batch rebuilds
/// the visible messages; only a block still growing is highlighted again.
final _spans = <(String, String, Brightness), TextSpan>{};
const _cached = 64;

/// The registered language [name] means: a fence's info string (its first
/// word, e.g. `rust,ignore` or `json title=x`) or a file extension.
String? _language(String? name) {
  final word = (name ?? '').trim().split(RegExp(r'[\s,{]')).first.toLowerCase();
  final language = _aliases[word] ?? word;
  return language.isNotEmpty && _highlight.getLanguage(language) != null
      ? language
      : null;
}

TextSpan? _highlighted(String code, String language, Brightness brightness) {
  final key = (language, code, brightness);
  final hit = _spans.remove(key);
  if (hit != null) return _spans[key] = hit;
  final TextSpan? span;
  // A parser port run over whatever a model wrote: on any failure the code
  // shows plain rather than breaking the message.
  try {
    final renderer = TextSpanRenderer(null, _themes[brightness]!);
    _highlight.highlight(code: code, language: language).render(renderer);
    span = renderer.span;
  } catch (_) {
    return null;
  }
  if (span == null) return null;
  _spans[key] = span;
  if (_spans.length > _cached) _spans.remove(_spans.keys.first);
  return span;
}

/// Selectable source code in [style], coloured by syntax when [language]
/// names a known one. Every line is as tall as the style's, whatever font a
/// glyph falls back to, so lines stay level with numbers beside them.
class CodeText extends StatelessWidget {
  final String code;
  final String? language;
  final TextStyle style;
  const CodeText(this.code, {super.key, this.language, required this.style});

  @override
  Widget build(BuildContext context) {
    final language = code.length <= _maxHighlighted
        ? _language(this.language)
        : null;
    final span = language == null
        ? null
        : _highlighted(code, language, Theme.of(context).brightness);
    final strut = StrutStyle.fromTextStyle(style, forceStrutHeight: true);
    return span == null
        ? SelectableText(code, style: style, strutStyle: strut)
        : SelectableText.rich(
            TextSpan(style: style, children: [span]),
            strutStyle: strut,
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
  const CodeBlock({
    super.key,
    required this.code,
    required this.info,
    required this.color,
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
      margin: const EdgeInsets.symmetric(vertical: 10),
      decoration: BoxDecoration(
        color: c.code,
        borderRadius: BorderRadius.circular(14),
        border: Border.all(color: c.border),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(14, 2, 4, 0),
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
                  padding: const EdgeInsets.only(left: 14),
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
                          padding: const EdgeInsets.fromLTRB(0, 0, 14, 14),
                          child: CodeText(
                            text,
                            language: widget.info,
                            style: style,
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

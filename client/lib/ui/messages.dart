import 'dart:convert';
import 'dart:math';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';
import 'package:markdown/markdown.dart' as md;

import '../l10n.dart';
import '../theme.dart';
import 'common.dart';

/// Message actions keep a 16px glyph in a compact highlight.
const _actionSize = 28.0;

/// Space between an action button's edge and its 16px glyph.
const _actionInset = (_actionSize - 16) / 2;

/// Item kinds a run produces on its way to an answer.
const _stepKinds = {'tool', 'reasoning', 'plan', 'file_change'};

/// The conversation as displayed. A run's answer is its last reply when
/// nothing else of the run follows it; the work before it (tool calls,
/// reasoning, interim replies) becomes one `steps` row. Memory context and
/// what a retried model call discarded are not shown.
List<Map<String, dynamic>> timeline(Iterable<Map<String, dynamic>> items) {
  final visible = items
      .where((i) => i['kind'] != 'memory_context' && i['state'] != 'discarded')
      .toList();
  bool ofRun(Map<String, dynamic> item) =>
      (item['run_id'] ?? '') != '' &&
      (item['kind'] == 'assistant' || _stepKinds.contains(item['kind']));
  final last = <String, Map<String, dynamic>>{
    for (final item in visible)
      if (ofRun(item)) item['run_id'] as String: item,
  };
  final rows = <Map<String, dynamic>>[];
  for (final item in visible) {
    final run = item['run_id'];
    if (!ofRun(item) ||
        item['kind'] == 'assistant' && identical(last[run], item)) {
      rows.add(item);
      continue;
    }
    // A step that only called tools leaves an empty reply.
    if (item['kind'] == 'assistant' && '${item['text'] ?? ''}'.trim().isEmpty) {
      continue;
    }
    final group = rows.lastOrNull;
    if (group?['kind'] == 'steps' && group!['run_id'] == run) {
      (group['steps'] as List<Map<String, dynamic>>).add(item);
    } else {
      rows.add({
        'id': 'steps:${item['id']}',
        'kind': 'steps',
        'run_id': run,
        'steps': [item],
      });
    }
  }
  for (final row in rows.where((r) => r['kind'] == 'steps')) {
    row['latest'] = identical((row['steps'] as List).last, last[row['run_id']]);
  }
  return rows;
}

List<Widget> _blocks(
  BuildContext context,
  String text,
  double size, {
  ValueChanged<String>? onSource,
  Color? color,
  _Citations? citations,
}) {
  final c = HubColors.of(context);
  final theme = Theme.of(context);
  final ink = color ?? c.text;
  final sheet = MarkdownStyleSheet.fromTheme(theme).copyWith(
    p: TextStyle(fontSize: size, height: 1.65, color: ink),
    listBullet: TextStyle(fontSize: size, height: 1.65, color: ink),
    code: TextStyle(
      fontFamily: 'monospace',
      fontFamilyFallback: const ['RyzHub Sans'],
      fontSize: size - 1.5,
      backgroundColor: c.code,
      color: ink,
    ),
    blockquoteDecoration: BoxDecoration(
      border: Border(left: BorderSide(color: c.border, width: 3)),
    ),
    horizontalRuleDecoration: BoxDecoration(
      border: Border(top: BorderSide(color: c.border)),
    ),
  );
  final chips = citations != null;
  Widget markdown(String data, {bool tail = false}) => MarkdownBody(
    data: chips ? citations.fold(data, tail: tail) : data,
    onTapLink: (_, href, _) {
      if (href != null) onSource?.call(href);
    },
    selectable: true,
    styleSheet: sheet,
    inlineSyntaxes: [if (chips) _ChipSyntax(), _FileLinkSyntax()],
    builders: {
      if (chips) 'cite': _ChipBuilder(citations, onSource),
      'file': _FileLinkBuilder(onSource),
    },
  );
  final out = <Widget>[];
  var cursor = 0;
  for (final code in RegExp(
    r'```([^\n]*)\n([\s\S]*?)(?:```|$)',
  ).allMatches(text)) {
    if (code.start > cursor) {
      out.add(markdown(text.substring(cursor, code.start)));
    }
    out.add(
      Container(
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
                      code.group(1)!,
                      style: TextStyle(fontSize: 12, color: c.subtle),
                    ),
                  ),
                  IconButton(
                    tooltip: tr('复制代码', 'Copy code'),
                    icon: const Icon(Icons.copy_rounded, size: 15),
                    onPressed: () =>
                        Clipboard.setData(ClipboardData(text: code.group(2)!)),
                  ),
                ],
              ),
            ),
            Padding(
              padding: const EdgeInsets.fromLTRB(14, 0, 14, 14),
              child: SelectableText(
                code.group(2)!,
                style: TextStyle(
                  fontFamily: 'monospace',
                  fontFamilyFallback: const ['RyzHub Sans'],
                  fontSize: 13.5,
                  height: 1.5,
                  color: ink,
                ),
              ),
            ),
          ],
        ),
      ),
    );
    cursor = code.end;
  }
  if (cursor < text.length) {
    out.add(markdown(text.substring(cursor), tail: true));
  }
  return out;
}

/// Delimit a chip in the markdown handed to the renderer: private-use code
/// points that no reply contains.
const _chipOpen = '\uE000', _chipClose = '\uE001';

/// The web pages a reply cites, and its source links folded into chips. Models
/// write a passage's sources after it as `([site](url))`; a run of such links
/// to cited pages becomes one chip, the rest of the text stays as is. The
/// server sends each cited page as it streams.
class _Citations {
  /// Cited pages by URL, in order of first citation.
  final Map<String, Map> byUrl;

  /// The reply is still streaming.
  final bool partial;

  /// Each chip's sources as (link label, page).
  final chips = <List<(String, Map)>>[];

  _Citations._(this.byUrl, this.partial);

  factory _Citations.of(Map<String, dynamic> item) {
    final list = item['detail']?['kind']?['Answer']?['citations'];
    return _Citations._({
      if (list is List)
        for (final citation in list.whereType<Map>())
          '${citation['url']}': citation,
    }, item['state'] == 'streaming');
  }

  /// [text] with each run of source links replaced by a chip placeholder. At
  /// the [tail] of a streaming reply, a run not yet finished is hidden.
  String fold(String text, {bool tail = false}) {
    final out = StringBuffer();
    var cursor = 0;
    for (var at = text.indexOf('(['); at >= 0; at = text.indexOf('([', at)) {
      final first = _group(text, at);
      final cited =
          first != null &&
          first.$1 >= 0 &&
          first.$2.every((link) => byUrl.containsKey(link.$2));
      if (!cited) {
        // At the tail of a streaming reply, links wait to be finished and cited.
        final waiting =
            first != null &&
            (first.$1 < 0 || text.substring(first.$1).trim().isEmpty);
        if (waiting && partial && tail) {
          return (out..write(text.substring(cursor, at))).toString();
        }
        at++;
        continue;
      }
      var (end, links) = first;
      // Groups apart by spaces only read as one.
      while (true) {
        var next = end;
        while (next < text.length && text[next] == ' ') {
          next++;
        }
        final more = _group(text, next);
        if (more == null ||
            more.$1 < 0 ||
            !more.$2.every((link) => byUrl.containsKey(link.$2))) {
          break;
        }
        (end, links) = (more.$1, [...links, ...more.$2]);
      }
      final seen = <String>{};
      chips.add([
        for (final (label, url) in links)
          if (seen.add(url)) (label, byUrl[url]!),
      ]);
      out
        ..write(text.substring(cursor, at))
        ..write('$_chipOpen${chips.length - 1}$_chipClose');
      cursor = at = end;
    }
    return (out..write(text.substring(cursor))).toString();
  }

  /// The web links of a `([label](url), [label](url))` at [start], and its
  /// end; the end is -1 when the text stops inside it.
  (int, List<(String, String)>)? _group(String text, int start) {
    if (!text.startsWith('([', start)) return null;
    const unfinished = (-1, <(String, String)>[]);
    final links = <(String, String)>[];
    var i = start + 1;
    while (true) {
      if (i >= text.length) return unfinished;
      if (text[i] != '[') return null;
      final close = text.indexOf('](', i);
      final line = text.indexOf('\n', i);
      if (close < 0) return line < 0 ? unfinished : null;
      if (line >= 0 && line < close) return null;
      // The link ends at the parenthesis that closes it; ones in the URL pair up.
      var depth = 0, j = close + 2;
      for (; j < text.length; j++) {
        final char = text[j];
        if (char == ' ' || char == '\n') return null;
        if (char == '(') depth++;
        if (char == ')' && depth-- == 0) break;
      }
      if (j >= text.length) return unfinished;
      final url = text.substring(close + 2, j);
      if (!url.startsWith('https://') && !url.startsWith('http://')) {
        return null;
      }
      links.add((text.substring(i + 1, close), url));
      i = j + 1;
      while (i < text.length && ', ;，；'.contains(text[i])) {
        i++;
      }
      if (i < text.length && text[i] == ')') return (i + 1, links);
    }
  }

  /// Cited pages no chip shows, in order of first citation.
  List<Map> get unshown {
    final shown = {
      for (final chip in chips)
        for (final (_, source) in chip) source['url'],
    };
    return [
      for (final source in byUrl.values)
        if (!shown.contains(source['url'])) source,
    ];
  }
}

/// [href] as the server records the files replies link to: without
/// `sandbox:` and percent-decoded.
String fileLinkPath(String href) {
  final path = href.startsWith('sandbox:') ? href.substring(8) : href;
  try {
    return Uri.decodeComponent(path);
  } on ArgumentError {
    return path;
  }
}

/// What a file link resolves to in [replies], the first that recorded it:
/// the files handed over with that reply and which one is linked, or [error]
/// saying why it was not handed over. Null when none recorded it.
({List<Map<String, dynamic>> files, int index, String? error})? linkedFile(
  String href,
  Iterable<dynamic> replies,
) {
  final link = fileLinkPath(href);
  for (final reply in replies) {
    if (reply is! Map) continue;
    final recorded = reply['detail']?['kind']?['Answer']?['files'];
    if (recorded is! List) continue;
    final shared = recorded.whereType<Map>().toList();
    final found = shared.where((f) => f['link'] == link).firstOrNull;
    if (found == null) continue;
    final files = [
      for (final f in shared)
        if (f['file'] is Map) Map<String, dynamic>.from(f['file'] as Map),
    ];
    final file = found['file'];
    if (file is! Map) {
      return (files: files, index: -1, error: '${found['error']}');
    }
    return (
      files: files,
      index: files.indexWhere((f) => f['id'] == file['id']),
      error: null,
    );
  }
  return null;
}

/// Whether [href] names a file of the sandbox rather than a page.
bool isFileLink(String href) {
  final path = fileLinkPath(href);
  return path.startsWith('/workspace/') || path.startsWith('/attachments/');
}

/// A link to a sandbox file, as `[label](sandbox:/workspace/...)`,
/// `[label](/attachments/...)` or with the path in `<...>`.
class _FileLinkSyntax extends md.InlineSyntax {
  _FileLinkSyntax()
    : super(
        r'\[([^\]\n]+)\]\((?:<((?:sandbox:)?/(?:workspace|attachments)/[^>\n]+)>'
        r'|((?:sandbox:)?/(?:workspace|attachments)/(?:[^()\s]|\([^()\s]*\))+))'
        r'(?:\s+"[^"\n]*")?\)',
      );

  @override
  bool onMatch(md.InlineParser parser, Match match) {
    parser.addNode(
      md.Element.text('file', match[1]!)
        ..attributes['href'] = match[2] ?? match[3]!,
    );
    return true;
  }
}

/// A file link inline: the file's glyph before the label, opened on tap.
class _FileLinkBuilder extends MarkdownElementBuilder {
  final ValueChanged<String>? onSource;
  _FileLinkBuilder(this.onSource);

  @override
  Widget? visitElementAfterWithContext(
    BuildContext context,
    md.Element element,
    TextStyle? preferredStyle,
    TextStyle? parentStyle,
  ) {
    final c = HubColors.of(context);
    final href = element.attributes['href'] ?? '';
    final style = (parentStyle ?? const TextStyle()).copyWith(color: c.accent);
    return Text.rich(
      TextSpan(
        children: [
          WidgetSpan(
            alignment: PlaceholderAlignment.middle,
            child: Semantics(
              link: true,
              child: InkWell(
                borderRadius: BorderRadius.circular(4),
                onTap: () => onSource?.call(href),
                child: Row(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Icon(
                      Icons.insert_drive_file_outlined,
                      size: (style.fontSize ?? 15) * .95,
                      color: c.accent,
                    ),
                    const SizedBox(width: 2),
                    Text(element.textContent, style: style),
                  ],
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }
}

class _ChipSyntax extends md.InlineSyntax {
  _ChipSyntax() : super('$_chipOpen(\\d+)$_chipClose');

  @override
  bool onMatch(md.InlineParser parser, Match match) {
    parser.addNode(md.Element.text('cite', match[1]!));
    return true;
  }
}

/// Renders a chip placeholder inline, so the paragraph wraps around it.
class _ChipBuilder extends MarkdownElementBuilder {
  final _Citations citations;
  final ValueChanged<String>? onSource;
  _ChipBuilder(this.citations, this.onSource);

  @override
  Widget? visitElementAfterWithContext(
    BuildContext context,
    md.Element element,
    TextStyle? preferredStyle,
    TextStyle? parentStyle,
  ) {
    final index = int.tryParse(element.textContent) ?? -1;
    final sources = citations.chips.elementAtOrNull(index);
    if (sources == null) return null;
    return Text.rich(
      TextSpan(
        children: [
          WidgetSpan(
            alignment: PlaceholderAlignment.middle,
            child: _CitationChip(sources: sources, onSource: onSource),
          ),
        ],
      ),
    );
  }
}

/// A cited page's title, or its site when it has none.
String _sourceTitle(Map source) {
  final title = '${source['title'] ?? ''}'.trim();
  final url = '${source['url'] ?? ''}';
  return title.isNotEmpty ? title : Uri.tryParse(url)?.host ?? url;
}

/// The first source's site, and how many more the chip stands for. One source
/// opens directly; several offer a choice.
class _CitationChip extends StatelessWidget {
  final List<(String, Map)> sources;
  final ValueChanged<String>? onSource;
  const _CitationChip({required this.sources, this.onSource});

  Future<void> _open(BuildContext context) async {
    final open = onSource;
    if (open == null) return;
    if (sources.length == 1) return open('${sources.first.$2['url']}');
    final box = context.findRenderObject()! as RenderBox;
    final overlay =
        Overlay.of(context).context.findRenderObject()! as RenderBox;
    final url = await showMenu<String>(
      context: context,
      position: RelativeRect.fromRect(
        box.localToGlobal(Offset(0, box.size.height), ancestor: overlay) &
            box.size,
        Offset.zero & overlay.size,
      ),
      items: [
        for (final (_, source) in sources)
          PopupMenuItem(
            value: '${source['url']}',
            child: Text(
              _sourceTitle(source),
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
            ),
          ),
      ],
    );
    if (url != null) open(url);
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final (label, first) = sources.first;
    final site = label.trim().isEmpty ? _sourceTitle(first) : label.trim();
    final more = sources.length - 1;
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 3),
      child: Tooltip(
        message: sources.map((s) => _sourceTitle(s.$2)).join('\n'),
        child: Material(
          color: c.code,
          shape: const StadiumBorder(),
          child: InkWell(
            customBorder: const StadiumBorder(),
            onTap: () => _open(context),
            child: Padding(
              padding: const EdgeInsets.symmetric(horizontal: 8),
              child: Text(
                more > 0 ? '$site +$more' : site,
                style: TextStyle(fontSize: 12, height: 1.6, color: c.muted),
              ),
            ),
          ),
        ),
      ),
    );
  }
}

/// The cited pages the reply's text does not link, each opened through
/// [onSource].
List<Widget> _sources(
  BuildContext context,
  _Citations citations,
  ValueChanged<String>? onSource,
) {
  final unshown = citations.unshown;
  if (unshown.isEmpty) return const [];
  final c = HubColors.of(context);
  return [
    Padding(
      padding: const EdgeInsets.only(top: 10, bottom: 2),
      child: Text(
        tr('来源', 'Sources'),
        style: TextStyle(fontSize: 12, color: c.subtle),
      ),
    ),
    for (final (i, source) in unshown.indexed)
      _Source(number: i + 1, source: source, onTap: onSource),
  ];
}

/// One cited page: its title and site. The cited passage shows on hover.
class _Source extends StatelessWidget {
  final int number;
  final Map source;
  final ValueChanged<String>? onTap;
  const _Source({required this.number, required this.source, this.onTap});

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final url = '${source['url'] ?? ''}';
    final site = Uri.tryParse(url)?.host ?? url;
    final title = '${source['title'] ?? ''}'.trim();
    final cited = '${source['cited_text'] ?? ''}'.trim();
    final row = InkWell(
      borderRadius: BorderRadius.circular(6),
      onTap: onTap == null ? null : () => onTap!(url),
      child: Padding(
        padding: const EdgeInsets.symmetric(vertical: 3),
        child: Text.rich(
          TextSpan(
            children: [
              TextSpan(
                text: '$number  ',
                style: TextStyle(color: c.subtle),
              ),
              TextSpan(text: title.isEmpty ? site : title),
              if (title.isNotEmpty)
                TextSpan(
                  text: '  $site',
                  style: TextStyle(fontSize: 12, color: c.subtle),
                ),
            ],
          ),
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: TextStyle(fontSize: 13, color: c.muted),
        ),
      ),
    );
    return cited.isEmpty ? row : Tooltip(message: cited, child: row);
  }
}

List<Widget> _states(BuildContext context, Map<String, dynamic> item) {
  final c = HubColors.of(context);
  final small = TextStyle(fontSize: 12, color: c.subtle);
  return [
    if (item['state'] == 'streaming')
      Padding(
        padding: const EdgeInsets.only(top: 6),
        child: Row(
          children: [
            SizedBox(
              width: 10,
              height: 10,
              child: CircularProgressIndicator(
                strokeWidth: 1.6,
                color: c.subtle,
              ),
            ),
            const SizedBox(width: 8),
            Text(tr('生成中…', 'Generating…'), style: small),
          ],
        ),
      ),
    if (item['state'] == 'invalidated')
      Text(
        tr(
          '已失效 · 此操作晚于恢复的检查点',
          'Invalidated · newer than the restored checkpoint',
        ),
        style: TextStyle(fontSize: 12, color: c.danger),
      ),
    if (item['state'] == 'cancelled') Text(tr('已停止', 'Stopped'), style: small),
    if (item['kind'] == 'steer')
      Text(switch (item['state']) {
        'injected' ||
        'handed' ||
        'consumed' => tr('已转交执行器', 'Handed to the agent'),
        'waiting' => tr('等待注入点', 'Waiting for an injection point'),
        'cancelled' => tr(
          '任务已结束，此补充指令未采用',
          'The run ended; this instruction was not used',
        ),
        _ => tr(
          '已接收，等待下一次模型请求',
          'Received; waiting for the next model request',
        ),
      }, style: small),
  ];
}

class _Action extends StatelessWidget {
  final String tooltip;
  final IconData icon;
  final double size;
  final VoidCallback? onPressed;
  const _Action({
    required this.tooltip,
    required this.icon,
    this.size = 16,
    this.onPressed,
  });
  @override
  Widget build(BuildContext context) => IconButton(
    tooltip: tooltip,
    onPressed: onPressed,
    icon: Icon(icon, size: size),
    style: IconButton.styleFrom(
      padding: EdgeInsets.zero,
      minimumSize: const Size.square(_actionSize),
      maximumSize: const Size.square(_actionSize),
      tapTargetSize: MaterialTapTargetSize.shrinkWrap,
      shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(8)),
    ),
  );
}

/// Markdown as messages show it, e.g. for a previewed file.
class MarkdownText extends StatelessWidget {
  final String text;
  final double size;
  const MarkdownText(this.text, {super.key, this.size = 15});
  @override
  Widget build(BuildContext context) => Column(
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: _blocks(context, text, size),
  );
}

class MessageCard extends StatelessWidget {
  final Map<String, dynamic> item;
  final String? agentLabel;
  final VoidCallback? onFork;
  final VoidCallback? onMemoryUndo;
  final ValueChanged<String>? onSource;

  /// Shows the files a message carried.
  final Widget Function(List<dynamic> files)? files;
  const MessageCard({
    super.key,
    required this.item,
    this.agentLabel,
    this.onFork,
    this.onMemoryUndo,
    this.onSource,
    this.files,
  });

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final text = item['text'] as String;
    final kind = item['kind'];
    if (kind == 'memory_notice') {
      Map<String, dynamic> notice;
      try {
        notice = jsonDecode(text) as Map<String, dynamic>;
      } catch (_) {
        notice = {};
      }
      return Padding(
        padding: const EdgeInsets.symmetric(vertical: 6),
        child: Row(
          children: [
            const Icon(Icons.psychology_outlined, size: 16),
            const SizedBox(width: 8),
            Expanded(
              child: Text(
                notice['action'] == 'delete'
                    ? tr('记忆已删除', 'Memory deleted')
                    : tr('记忆已更新', 'Memory updated'),
                style: TextStyle(fontSize: 12, color: c.subtle),
              ),
            ),
            TextButton(onPressed: onMemoryUndo, child: Text(tr('撤销', 'Undo'))),
          ],
        ),
      );
    }
    final copy = _Action(
      tooltip: tr('复制消息', 'Copy message'),
      icon: Icons.copy_rounded,
      onPressed: () => Clipboard.setData(ClipboardData(text: text)),
    );
    final attached =
        item['detail']?['kind']?['Attachments']?['files'] as List? ?? const [];
    final carried = attached.isEmpty ? null : files?.call(attached);
    if (kind == 'user') {
      return Hover(
        // Full width so the bubble sits on the right, chat style; a loose
        // parent would otherwise shrink the column to the bubble and centre it.
        builder: (context, hovered) => Container(
          width: double.infinity,
          padding: const EdgeInsets.only(bottom: 8),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.end,
            children: [
              if (carried != null)
                Padding(
                  padding: EdgeInsets.only(bottom: text.isEmpty ? 0 : 6),
                  child: LayoutBuilder(
                    builder: (context, box) => ConstrainedBox(
                      constraints: BoxConstraints(maxWidth: box.maxWidth * .78),
                      child: carried,
                    ),
                  ),
                ),
              if (text.isNotEmpty)
                LayoutBuilder(
                  builder: (context, box) => ConstrainedBox(
                    constraints: BoxConstraints(maxWidth: box.maxWidth * .78),
                    child: Container(
                      padding: const EdgeInsets.fromLTRB(18, 10, 18, 10),
                      decoration: BoxDecoration(
                        color: c.bubble,
                        borderRadius: BorderRadius.circular(22),
                      ),
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        mainAxisSize: MainAxisSize.min,
                        children: [
                          ..._blocks(context, text, 15, onSource: onSource),
                          ..._states(context, item),
                        ],
                      ),
                    ),
                  ),
                ),
              const SizedBox(height: 6),
              // Its glyph lines up with the bubble's right edge.
              Transform.translate(
                offset: const Offset(_actionInset, 0),
                child: Reveal(visible: hovered, child: copy),
              ),
            ],
          ),
        ),
      );
    }
    if (kind == 'assistant') {
      final citations = _Citations.of(item);
      return Padding(
        padding: const EdgeInsets.only(bottom: 18),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            ..._blocks(
              context,
              text,
              15,
              onSource: onSource,
              citations: citations,
            ),
            ..._sources(context, citations, onSource),
            ..._states(context, item),
            // The actions mark the end of a reply, so they wait for it.
            if (item['state'] != 'streaming') ...[
              const SizedBox(height: 8),
              // Shifted by the button's inner margin so the first glyph lines
              // up with the reply's left edge; the highlight keeps its size.
              Transform.translate(
                offset: const Offset(-_actionInset, 0),
                child: Row(
                  children: [
                    copy,
                    if (onFork != null)
                      _Action(
                        tooltip: tr('从此处分叉', 'Fork from here'),
                        icon: Icons.fork_right,
                        size: 17,
                        onPressed: onFork,
                      ),
                    const SizedBox(width: 6),
                    Flexible(
                      child: Text(
                        agentLabel ?? tr('模拟 Agent', 'Mock agent'),
                        overflow: TextOverflow.ellipsis,
                        style: TextStyle(fontSize: 12, color: c.subtle),
                      ),
                    ),
                  ],
                ),
              ),
            ],
          ],
        ),
      );
    }
    final (icon, label) = switch (kind) {
      'tool' => (Icons.build_outlined, tr('工具', 'Tool')),
      'reasoning' => (Icons.lightbulb_outline, tr('思考', 'Reasoning')),
      'plan' => (Icons.checklist, tr('计划', 'Plan')),
      'file_change' => (Icons.difference_outlined, tr('文件变更', 'File change')),
      'steer' => (Icons.subdirectory_arrow_right, tr('补充指令', 'Instruction')),
      'side_query' => (Icons.visibility_outlined, tr('只读旁问', 'Side question')),
      'notice' => (Icons.info_outline, tr('系统通知', 'Notice')),
      _ => (
        Icons.auto_awesome_outlined,
        agentLabel ?? tr('模拟 Agent', 'Mock agent'),
      ),
    };
    return Padding(
      padding: const EdgeInsets.only(bottom: 14),
      child: Container(
        padding: const EdgeInsets.fromLTRB(14, 10, 8, 12),
        decoration: BoxDecoration(
          borderRadius: BorderRadius.circular(14),
          border: Border.all(color: c.border),
        ),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Row(
              children: [
                Icon(icon, size: 15, color: c.subtle),
                const SizedBox(width: 8),
                Text(label, style: TextStyle(fontSize: 13, color: c.muted)),
                const Spacer(),
                copy,
              ],
            ),
            Padding(
              padding: const EdgeInsets.only(right: 6),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  if (carried != null)
                    Padding(
                      padding: const EdgeInsets.only(top: 6, bottom: 4),
                      child: Align(
                        alignment: Alignment.centerLeft,
                        child: carried,
                      ),
                    ),
                  if (text.isNotEmpty)
                    ..._blocks(context, text, 14, onSource: onSource),
                  ..._states(context, item),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// A summary line that opens what it summarises. Hover and keyboard focus
/// darken it rather than shade it.
class _Disclosure extends StatefulWidget {
  final bool open;
  final VoidCallback? onTap;

  /// The leading icon; null shows a spinner.
  final IconData? icon;

  /// The line's text, darkened while `active`.
  final Widget Function(bool active) label;
  const _Disclosure({
    required this.open,
    required this.icon,
    required this.label,
    this.onTap,
  });
  @override
  State<_Disclosure> createState() => _DisclosureState();
}

class _DisclosureState extends State<_Disclosure> {
  bool hovered = false, focused = false;
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final active = (hovered || focused) && widget.onTap != null;
    final ink = active ? c.text : c.subtle;
    return Align(
      alignment: Alignment.centerLeft,
      child: InkWell(
        onTap: widget.onTap,
        onHover: (value) => setState(() => hovered = value),
        onFocusChange: (value) => setState(() => focused = value),
        overlayColor: const WidgetStatePropertyAll(Colors.transparent),
        splashFactory: NoSplash.splashFactory,
        child: Padding(
          padding: const EdgeInsets.symmetric(vertical: 4),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              SizedBox.square(
                dimension: 15,
                child: Center(
                  child: widget.icon == null
                      ? const _Spinner()
                      : Icon(widget.icon, size: 15, color: ink),
                ),
              ),
              const SizedBox(width: 10),
              Flexible(child: widget.label(active)),
              if (widget.onTap != null)
                Icon(
                  widget.open ? Icons.expand_more : Icons.chevron_right,
                  size: 18,
                  color: ink,
                ),
            ],
          ),
        ),
      ),
    );
  }
}

class _Spinner extends StatelessWidget {
  const _Spinner();
  @override
  Widget build(BuildContext context) => SizedBox.square(
    dimension: 10,
    child: CircularProgressIndicator(
      strokeWidth: 1.6,
      color: HubColors.of(context).subtle,
    ),
  );
}

/// What a run did before its answer, folded under one summary line. It stays
/// open while the run works and folds when the run ends, unless the user has
/// opened or closed it.
class StepsGroup extends StatefulWidget {
  final List<Map<String, dynamic>> steps;

  /// The run has not ended.
  final bool running;

  /// Nothing of the run follows these steps yet.
  final bool latest;

  /// How long the finished run worked, when known.
  final Duration? worked;
  final ValueChanged<String>? onSource;
  const StepsGroup({
    super.key,
    required this.steps,
    this.running = false,
    this.latest = false,
    this.worked,
    this.onSource,
  });
  @override
  State<StepsGroup> createState() => _StepsGroupState();
}

class _StepsGroupState extends State<StepsGroup> {
  /// The user's choice; null follows the run.
  bool? open;

  String get summary =>
      _did(widget.steps) ??
      (widget.steps.any((s) => s['kind'] == 'reasoning')
          ? tr('已思考', 'Reasoned')
          : tr('已处理', 'Worked'));

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final expanded = open ?? widget.running;
    final working = widget.running && widget.latest;
    // Once the model has spoken between its steps, each run of calls folds
    // under what it did and the group tells how long the run worked.
    final spoke = widget.steps.any((s) => s['kind'] == 'assistant');
    final blocks = <List<Map<String, dynamic>>>[];
    for (final s in widget.steps) {
      if (spoke && _folds(s) && blocks.isNotEmpty && _folds(blocks.last[0])) {
        blocks.last.add(s);
      } else {
        blocks.add([s]);
      }
    }
    return Padding(
      padding: const EdgeInsets.only(bottom: 14),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          _Disclosure(
            open: expanded,
            onTap: () => setState(() => open = !expanded),
            icon: working ? null : Icons.auto_awesome_outlined,
            label: (active) => Text(
              working
                  ? tr('处理中…', 'Working…')
                  : widget.worked == null
                  ? spoke
                        ? tr('已处理', 'Worked')
                        : summary
                  // Folds name the calls once the model has spoken.
                  : _worked(widget.worked!, spoke ? [] : _done(widget.steps)),
              style: TextStyle(
                fontSize: 13.5,
                color: active ? c.text : c.subtle,
              ),
            ),
          ),
          Divider(height: 9, thickness: 1, color: c.border),
          AnimatedSize(
            duration: const Duration(milliseconds: 160),
            alignment: Alignment.topCenter,
            child: expanded
                ? Padding(
                    padding: const EdgeInsets.only(top: 6),
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.stretch,
                      children: [
                        for (final (i, block) in blocks.indexed)
                          spoke && _folds(block[0])
                              ? _StepFold(
                                  key: ValueKey('fold:${block[0]['id']}'),
                                  steps: block,
                                  active: working && i == blocks.length - 1,
                                  step: (item) => step(context, item),
                                )
                              : step(context, block[0]),
                      ],
                    ),
                  )
                : const SizedBox(width: double.infinity),
          ),
        ],
      ),
    );
  }

  Widget step(BuildContext context, Map<String, dynamic> item) {
    final c = HubColors.of(context);
    final text = '${item['text'] ?? ''}';
    Widget note(IconData icon, List<Widget> body) => Padding(
      padding: const EdgeInsets.symmetric(vertical: 4),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Padding(
            padding: const EdgeInsets.only(top: 4),
            child: Icon(icon, size: 15, color: c.subtle),
          ),
          const SizedBox(width: 10),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: body,
            ),
          ),
        ],
      ),
    );
    final muted = TextStyle(fontSize: 13.5, height: 1.65, color: c.muted);
    final citations = _Citations.of(item);
    return switch (item['kind']) {
      'tool' => _ToolStep(
        key: ValueKey(item['id']),
        item: item,
        running: widget.running,
      ),
      'reasoning' => note(
        Icons.lightbulb_outline,
        _blocks(context, text, 14, onSource: widget.onSource, color: c.muted),
      ),
      'plan' => note(
        Icons.checklist,
        _blocks(context, text, 14, onSource: widget.onSource, color: c.muted),
      ),
      'file_change' => note(Icons.difference_outlined, [
        for (final (change, path) in _changes(item))
          Text(switch (change) {
            'add' => tr('新增 $path', 'Added $path'),
            'delete' => tr('删除 $path', 'Deleted $path'),
            _ => tr('修改 $path', 'Edited $path'),
          }, style: muted),
        if (_changes(item).isEmpty) Text(text, style: muted),
      ]),
      // An interim reply reads like the answer it leads to.
      _ => Padding(
        padding: const EdgeInsets.symmetric(vertical: 4),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            ..._blocks(
              context,
              text,
              15,
              onSource: widget.onSource,
              citations: citations,
            ),
            ..._sources(context, citations, widget.onSource),
            ..._states(context, item),
          ],
        ),
      ),
    };
  }
}

/// Steps that fold together between the model's words: calls and file edits.
bool _folds(Map<String, dynamic> step) =>
    step['kind'] == 'tool' || step['kind'] == 'file_change';

/// Chat's command tools. A Work command carries its command line as its name
/// and no arguments.
const _commandTools = {'exec', 'exec_poll', 'exec_write', 'exec_kill'};

bool _isCommand(Map<String, dynamic> step) {
  final tool = step['detail']?['kind']?['Tool'];
  if (tool is! Map) return false;
  final name = '${tool['name'] ?? ''}';
  return _commandTools.contains(name) ||
      name.isNotEmpty &&
          name != 'web_search' &&
          '${tool['arguments_json'] ?? ''}'.isEmpty;
}

/// What [steps] did as (Chinese, English) phrases, e.g. ("搜索网页 2 次",
/// "searched the web twice"); none when they called nothing and changed no
/// file.
List<(String, String)> _done(Iterable<Map<String, dynamic>> steps) {
  final calls = steps.where((s) => s['kind'] == 'tool').toList();
  final searches = calls
      .where((s) => _ToolCall.of(s).name == 'web_search')
      .length;
  final commands = calls.where(_isCommand).length;
  final images = calls
      .where((s) => _ToolCall.of(s).name == 'view_image')
      .length;
  final tools = calls.length - searches - commands - images;
  final files = {
    for (final step in steps.where((s) => s['kind'] == 'file_change'))
      ...switch (_changes(step)) {
        [] => ['${step['id']}'],
        final changes => changes.map((c) => c.$2),
      },
  }.length;
  String count(int n, String noun) => n == 1 ? '1 $noun' : '$n ${noun}s';
  return [
    if (searches > 0)
      (
        '搜索网页 $searches 次',
        'searched the web ${switch (searches) {
          1 => 'once',
          2 => 'twice',
          _ => '$searches times',
        }}',
      ),
    if (commands > 0) ('运行 $commands 条命令', 'ran ${count(commands, 'command')}'),
    if (images > 0) ('查看 $images 张图片', 'viewed ${count(images, 'image')}'),
    if (tools > 0) ('调用 $tools 个工具', 'used ${count(tools, 'tool')}'),
    if (files > 0) ('修改 $files 个文件', 'edited ${count(files, 'file')}'),
  ];
}

/// What [steps] did, e.g. "Searched the web twice, ran 1 command"; null when
/// they called nothing and changed no file.
String? _did(Iterable<Map<String, dynamic>> steps) {
  final done = _done(steps);
  if (done.isEmpty) return null;
  final en = done.map((d) => d.$2).join(', ');
  return tr(
    '已${done.map((d) => d.$1).join('，')}',
    en[0].toUpperCase() + en.substring(1),
  );
}

/// How long a run worked, then what it [did], e.g. "Worked for 1m 5s,
/// searched the web twice".
String _worked(Duration worked, [List<(String, String)> did = const []]) {
  final seconds = max(1, worked.inSeconds);
  final (h, m, s) = (seconds ~/ 3600, seconds % 3600 ~/ 60, seconds % 60);
  final zh = h > 0
      ? '$h 小时 $m 分'
      : m > 0
      ? '$m 分 $s 秒'
      : '$s 秒';
  final en = h > 0
      ? '${h}h ${m}m'
      : m > 0
      ? '${m}m ${s}s'
      : '${s}s';
  return tr(
    ['已工作 $zh', ...did.map((d) => d.$1)].join('，'),
    ['Worked for $en', ...did.map((d) => d.$2)].join(', '),
  );
}

/// The icon of what a fold's steps have in common.
IconData _foldIcon(List<Map<String, dynamic>> steps) {
  if (steps.every((s) => s['kind'] == 'file_change')) {
    return Icons.difference_outlined;
  }
  if (steps.every(_isCommand)) return Icons.terminal;
  final names = {for (final s in steps) _ToolCall.of(s).name};
  return names.length == 1 && steps.every((s) => s['kind'] == 'tool')
      ? _ToolCall(names.first, null, null).icon
      : Icons.build_outlined;
}

/// Calls the model made between its words, folded under what they did. The
/// fold the run is at work in stays open.
class _StepFold extends StatefulWidget {
  final List<Map<String, dynamic>> steps;
  final bool active;
  final Widget Function(Map<String, dynamic> item) step;
  const _StepFold({
    super.key,
    required this.steps,
    required this.active,
    required this.step,
  });
  @override
  State<_StepFold> createState() => _StepFoldState();
}

class _StepFoldState extends State<_StepFold> {
  /// The user's choice; null follows the run.
  bool? open;

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final expanded = open ?? widget.active;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _Disclosure(
          open: expanded,
          onTap: () => setState(() => open = !expanded),
          icon: widget.active ? null : _foldIcon(widget.steps),
          label: (active) => Text(
            _did(widget.steps) ?? tr('已处理', 'Worked'),
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: TextStyle(fontSize: 13.5, color: active ? c.text : c.muted),
          ),
        ),
        AnimatedSize(
          duration: const Duration(milliseconds: 160),
          alignment: Alignment.topCenter,
          // Indented so the calls' icons line up under the fold's text.
          child: expanded
              ? Padding(
                  padding: const EdgeInsets.only(left: 25),
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.stretch,
                    children: [for (final s in widget.steps) widget.step(s)],
                  ),
                )
              : const SizedBox(width: double.infinity),
        ),
      ],
    );
  }
}

/// A file-change item's (change, path) pairs.
List<(String, String)> _changes(Map<String, dynamic> item) {
  final changes = item['detail']?['kind']?['FileChange']?['changes'];
  return [
    if (changes is List)
      for (final change in changes)
        if (change is Map) ('${change['change']}', '${change['path']}'),
  ];
}

Object? _decode(Object? text) {
  if (text is! String || text.isEmpty) return null;
  try {
    return jsonDecode(text);
  } on FormatException {
    return null;
  }
}

/// How far a tool call got: running, done, or stopped short (failed, denied
/// or cut off by the run's end).
enum _Phase { running, done, stopped }

/// A tool item's call, read from its `detail`. Items without one carry only
/// the v1 text: `name\narguments` while planned, the result JSON once done.
class _ToolCall {
  final String name, output;
  final Map<String, dynamic> args;
  final Object? result;
  _ToolCall(this.name, Object? args, this.result, [this.output = ''])
    : args = args is Map<String, dynamic> ? args : const {};

  factory _ToolCall.of(Map<String, dynamic> item) {
    final tool = item['detail']?['kind']?['Tool'];
    if (tool is Map) {
      return _ToolCall(
        '${tool['name'] ?? ''}',
        _decode(tool['arguments_json']),
        _decode(tool['result_json']),
        '${tool['output'] ?? ''}',
      );
    }
    final text = '${item['text'] ?? ''}';
    final result = _decode(text);
    if (result != null) return _ToolCall('', null, result);
    final cut = text.indexOf('\n');
    return cut < 0
        ? _ToolCall(text, null, null)
        : _ToolCall(
            text.substring(0, cut),
            _decode(text.substring(cut + 1)),
            null,
          );
  }

  /// Built-in Chat tools: icon, the action in Chinese, and in English as
  /// (plain, in progress, done).
  static const _builtIn = <String, (IconData, String, String, String, String)>{
    'memory_search': (
      Icons.search,
      '搜索记忆',
      'Search memories',
      'Searching memories',
      'Searched memories',
    ),
    'conversation_search': (
      Icons.search,
      '搜索历史对话',
      'Search past chats',
      'Searching past chats',
      'Searched past chats',
    ),
    'project_file_search': (
      Icons.search,
      '搜索项目文件',
      'Search project files',
      'Searching project files',
      'Searched project files',
    ),
    'memory_save': (
      Icons.bookmark_add_outlined,
      '保存记忆',
      'Save a memory',
      'Saving a memory',
      'Saved a memory',
    ),
    'memory_forget': (
      Icons.bookmark_remove_outlined,
      '删除记忆',
      'Forget a memory',
      'Forgetting a memory',
      'Forgot a memory',
    ),
    'exec': (
      Icons.terminal,
      '运行命令',
      'Run command',
      'Running command',
      'Ran command',
    ),
    'exec_poll': (
      Icons.terminal,
      '查看后台任务',
      'Check a background job',
      'Checking a background job',
      'Checked a background job',
    ),
    'exec_write': (
      Icons.terminal,
      '向后台任务输入',
      'Write to a background job',
      'Writing to a background job',
      'Wrote to a background job',
    ),
    'exec_kill': (
      Icons.terminal,
      '停止后台任务',
      'Stop a background job',
      'Stopping a background job',
      'Stopped a background job',
    ),
    'view_image': (
      Icons.image_outlined,
      '查看图片',
      'View an image',
      'Viewing an image',
      'Viewed an image',
    ),
    'ask_user': (
      Icons.help_outline,
      '向你提问',
      'Ask you',
      'Asking you',
      'Asked you',
    ),
    // The provider's own search.
    'web_search': (
      Icons.travel_explore,
      '搜索网页',
      'Search the web',
      'Searching the web',
      'Searched the web',
    ),
  };

  IconData get icon => _builtIn[name]?.$1 ?? Icons.build_outlined;

  /// What the call does, worded for how far it got. Other tools show their
  /// own name.
  String label(_Phase phase) {
    final builtIn = _builtIn[name];
    if (builtIn == null) return name.isEmpty ? tr('工具', 'Tool') : name;
    final (_, zh, plain, going, done) = builtIn;
    return switch (phase) {
      _Phase.running => tr('正在$zh', going),
      _Phase.done => tr('已$zh', done),
      _Phase.stopped => tr(zh, plain),
    };
  }

  /// What the call acted on, with its leading separator.
  String get subject {
    final value =
        args[switch (name) {
          'memory_search' ||
          'conversation_search' ||
          'project_file_search' ||
          'web_search' => 'query',
          'memory_save' => 'content',
          'ask_user' => 'question',
          'exec' => 'cmd',
          'view_image' => 'path',
          _ => '',
        }] ??
        // A search that opened a page names the page instead.
        (name == 'web_search' ? args['url'] : null);
    if (value is! String || value.isEmpty) return '';
    // Full-width quotes bring their own space in Chinese.
    return name == 'exec' || name == 'view_image'
        ? ' $value'
        : tr('“$value”', ' “$value”');
  }

  /// How the call ended, when that is worth a word, and whether it failed.
  (String, bool)? get outcome {
    final result = this.result;
    if (result is! Map) return null;
    final error = result['error'];
    if (error == 'user_denied') return (tr('已拒绝', 'Denied'), true);
    if (error != null) return (tr('失败', 'Failed'), true);
    final code = result['exit_code'];
    if (code is int && code != 0) {
      return (tr('退出码 $code', 'Exit code $code'), true);
    }
    final matches = result['matches'];
    if (matches is List) {
      final n = matches.length;
      return (
        n == 0
            ? tr('无结果', 'No results')
            : tr('$n 条结果', n == 1 ? '1 result' : '$n results'),
        false,
      );
    }
    return null;
  }

  /// What the user can open: the call itself and what came back.
  List<(String, String)> get details {
    const pretty = JsonEncoder.withIndent('  ');
    return [
      if (name.isNotEmpty) (tr('工具', 'Tool'), name),
      if (args.isNotEmpty) (tr('参数', 'Arguments'), pretty.convert(args)),
      if (output.isNotEmpty)
        (tr('输出', 'Output'), output)
      else if (result != null)
        (tr('结果', 'Result'), pretty.convert(result)),
    ];
  }
}

/// One tool call as a summary line; its arguments and result open below.
class _ToolStep extends StatefulWidget {
  final Map<String, dynamic> item;
  final bool running;
  const _ToolStep({super.key, required this.item, required this.running});
  @override
  State<_ToolStep> createState() => _ToolStepState();
}

class _ToolStepState extends State<_ToolStep> {
  bool open = false;
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final call = _ToolCall.of(widget.item);
    final done = widget.item['state'] == 'completed';
    final outcome = done
        ? call.outcome
        : widget.running
        ? null
        : (tr('未完成', 'Unfinished'), false);
    final phase = !done && widget.running
        ? _Phase.running
        : done && outcome?.$2 != true
        ? _Phase.done
        : _Phase.stopped;
    final subject = call.subject;
    final details = call.details;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _Disclosure(
          open: open,
          onTap: details.isEmpty ? null : () => setState(() => open = !open),
          icon: !done && widget.running ? null : call.icon,
          label: (active) => Text.rich(
            TextSpan(
              children: [
                TextSpan(text: call.label(phase)),
                if (subject.isNotEmpty)
                  TextSpan(
                    text: subject,
                    style: TextStyle(color: active ? c.text : c.subtle),
                  ),
                if (outcome != null)
                  TextSpan(
                    text: ' · ${outcome.$1}',
                    style: TextStyle(
                      color: outcome.$2
                          ? c.danger
                          : active
                          ? c.text
                          : c.subtle,
                    ),
                  ),
              ],
            ),
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: TextStyle(fontSize: 13.5, color: active ? c.text : c.muted),
          ),
        ),
        if (open)
          Padding(
            padding: const EdgeInsets.fromLTRB(0, 2, 0, 6),
            child: Container(
              constraints: const BoxConstraints(maxHeight: 280),
              decoration: BoxDecoration(
                color: c.code,
                borderRadius: BorderRadius.circular(10),
                border: Border.all(color: c.border),
              ),
              child: SingleChildScrollView(
                padding: const EdgeInsets.fromLTRB(12, 8, 12, 10),
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    for (final (i, (title, body)) in details.indexed) ...[
                      if (i > 0) const SizedBox(height: 10),
                      Text(
                        title,
                        style: TextStyle(fontSize: 12, color: c.subtle),
                      ),
                      const SizedBox(height: 2),
                      SelectableText(
                        body,
                        style: TextStyle(
                          fontFamily: 'monospace',
                          fontFamilyFallback: const ['RyzHub Sans'],
                          fontSize: 12.5,
                          height: 1.45,
                          color: c.text,
                        ),
                      ),
                    ],
                  ],
                ),
              ),
            ),
          ),
      ],
    );
  }
}

class ApprovalCard extends StatelessWidget {
  final String summary;
  final VoidCallback onDeny, onAllow;
  const ApprovalCard({
    super.key,
    required this.summary,
    required this.onDeny,
    required this.onAllow,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Container(
      padding: const EdgeInsets.fromLTRB(16, 12, 12, 12),
      decoration: BoxDecoration(
        color: c.composer,
        borderRadius: BorderRadius.circular(18),
        border: Border.all(color: c.border),
      ),
      child: Row(
        children: [
          Icon(Icons.shield_outlined, color: c.accent),
          const SizedBox(width: 12),
          Expanded(
            child: Text(summary, style: TextStyle(color: c.text)),
          ),
          const SizedBox(width: 8),
          OutlinedButton(onPressed: onDeny, child: Text(tr('拒绝', 'Deny'))),
          const SizedBox(width: 8),
          FilledButton(onPressed: onAllow, child: Text(tr('允许', 'Allow'))),
        ],
      ),
    );
  }
}

class QuestionCard extends StatefulWidget {
  final String summary;
  final Future<void> Function(String) onAnswer;
  const QuestionCard({
    super.key,
    required this.summary,
    required this.onAnswer,
  });
  @override
  State<QuestionCard> createState() => _QuestionCardState();
}

class _QuestionCardState extends State<QuestionCard> {
  late List<Map<String, dynamic>> questions;
  late List<TextEditingController> inputs;
  bool busy = false;
  @override
  void initState() {
    super.initState();
    readQuestions();
  }

  void readQuestions() {
    questions = [
      {'question': widget.summary},
    ];
    try {
      final decoded = jsonDecode(widget.summary);
      if (decoded is List &&
          decoded.isNotEmpty &&
          decoded.every(
            (q) => q is Map && q['id'] is String && q['question'] is String,
          )) {
        questions = decoded
            .map((q) => Map<String, dynamic>.from(q as Map))
            .toList();
      }
    } on FormatException {
      // Chat questions are plain text; native harness questions are structured.
    }
    inputs = questions.map((_) => TextEditingController()).toList();
  }

  @override
  void didUpdateWidget(covariant QuestionCard oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.summary != widget.summary) {
      for (final input in inputs) {
        input.dispose();
      }
      readQuestions();
    }
  }

  @override
  void dispose() {
    for (final input in inputs) {
      input.dispose();
    }
    super.dispose();
  }

  Future<void> submit() async {
    if (busy ||
        inputs.any(
          (input) =>
              input.text.trim().isEmpty || !input.value.composing.isCollapsed,
        )) {
      return;
    }
    setState(() => busy = true);
    try {
      await widget.onAnswer(
        inputs.length == 1
            ? inputs.single.text
            : jsonEncode({
                for (var i = 0; i < questions.length; i++)
                  questions[i]['id'] as String: inputs[i].text,
              }),
      );
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Container(
      padding: const EdgeInsets.all(16),
      decoration: BoxDecoration(
        color: c.composer,
        borderRadius: BorderRadius.circular(18),
        border: Border.all(color: c.border),
      ),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          ConstrainedBox(
            constraints: BoxConstraints(
              maxHeight:
                  MediaQuery.sizeOf(context).height *
                  (MediaQuery.sizeOf(context).width < 600 ? .22 : .32),
            ),
            child: SingleChildScrollView(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  for (var i = 0; i < questions.length; i++) ...[
                    if (i > 0) const SizedBox(height: 16),
                    Row(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Icon(Icons.help_outline, size: 18, color: c.accent),
                        const SizedBox(width: 10),
                        Expanded(
                          child: Semantics(
                            container: true,
                            child: Text(
                              questions[i]['question'] as String,
                              style: TextStyle(color: c.text),
                            ),
                          ),
                        ),
                      ],
                    ),
                    if (questions[i]['options'] is List)
                      Padding(
                        padding: const EdgeInsets.only(top: 10),
                        child: Wrap(
                          spacing: 8,
                          runSpacing: 8,
                          children: [
                            for (final option
                                in questions[i]['options'] as List)
                              if (option is Map && option['label'] is String)
                                Tooltip(
                                  message: option['description'] is String
                                      ? option['description'] as String
                                      : '',
                                  child: OutlinedButton(
                                    onPressed: busy
                                        ? null
                                        : () => inputs[i].text =
                                              option['label'] as String,
                                    child: Text(option['label'] as String),
                                  ),
                                ),
                          ],
                        ),
                      ),
                    const SizedBox(height: 12),
                    TextField(
                      controller: inputs[i],
                      enabled: !busy,
                      decoration: InputDecoration(
                        labelText: inputs.length == 1
                            ? tr('回答', 'Answer')
                            : tr('回答 ${i + 1}', 'Answer ${i + 1}'),
                      ),
                    ),
                  ],
                ],
              ),
            ),
          ),
          const SizedBox(height: 10),
          Align(
            alignment: Alignment.centerRight,
            child: FilledButton(
              onPressed: busy ? null : submit,
              child: Text(tr('提交回答', 'Submit answer')),
            ),
          ),
        ],
      ),
    );
  }
}

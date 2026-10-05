import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';

import '../l10n.dart';
import '../theme.dart';
import 'common.dart';

/// Space between an action button's edge and its 16px glyph (40px button).
const _actionInset = 12.0;

class MessageCard extends StatelessWidget {
  final Map<String, dynamic> item;
  final String? agentLabel;
  final VoidCallback? onFork;
  final VoidCallback? onMemoryUndo;
  final Future<List<dynamic>> Function()? onMemoryRead;
  final ValueChanged<String>? onSource;
  const MessageCard({
    super.key,
    required this.item,
    this.agentLabel,
    this.onFork,
    this.onMemoryUndo,
    this.onMemoryRead,
    this.onSource,
  });

  List<Widget> blocks(BuildContext context, String text, double size) {
    final c = HubColors.of(context);
    final theme = Theme.of(context);
    final sheet = MarkdownStyleSheet.fromTheme(theme).copyWith(
      p: TextStyle(fontSize: size, height: 1.65, color: c.text),
      listBullet: TextStyle(fontSize: size, height: 1.65, color: c.text),
      code: TextStyle(
        fontFamily: 'monospace',
        fontFamilyFallback: const ['RyzHub Sans'],
        fontSize: size - 1.5,
        backgroundColor: c.code,
        color: c.text,
      ),
      blockquoteDecoration: BoxDecoration(
        border: Border(left: BorderSide(color: c.border, width: 3)),
      ),
      horizontalRuleDecoration: BoxDecoration(
        border: Border(top: BorderSide(color: c.border)),
      ),
    );
    final out = <Widget>[];
    var cursor = 0;
    for (final code in RegExp(
      r'```([^\n]*)\n([\s\S]*?)(?:```|$)',
    ).allMatches(text)) {
      if (code.start > cursor) {
        out.add(
          MarkdownBody(
            data: text.substring(cursor, code.start),
            onTapLink: (_, href, _) {
              if (href != null) onSource?.call(href);
            },
            selectable: true,
            styleSheet: sheet,
          ),
        );
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
                      onPressed: () => Clipboard.setData(
                        ClipboardData(text: code.group(2)!),
                      ),
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
                    color: c.text,
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
      out.add(
        MarkdownBody(
          data: text.substring(cursor),
          onTapLink: (_, href, _) {
            if (href != null) onSource?.call(href);
          },
          selectable: true,
          styleSheet: sheet,
        ),
      );
    }
    return out;
  }

  List<Widget> states(BuildContext context) {
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
      if (item['state'] == 'cancelled')
        Text(tr('已停止', 'Stopped'), style: small),
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

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final text = item['text'] as String;
    final kind = item['kind'];
    if (kind == 'memory_context') {
      return _MemoryContext(read: onMemoryRead, onSource: onSource);
    }
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
    final copy = IconButton(
      tooltip: tr('复制消息', 'Copy message'),
      onPressed: () => Clipboard.setData(ClipboardData(text: text)),
      icon: const Icon(Icons.copy_rounded, size: 16),
    );
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
                        ...blocks(context, text, 15),
                        ...states(context),
                      ],
                    ),
                  ),
                ),
              ),
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
      return Padding(
        padding: const EdgeInsets.only(bottom: 18),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            ...blocks(context, text, 15),
            ...states(context),
            const SizedBox(height: 2),
            // Shifted by the button's inner margin so the first glyph lines up
            // with the reply's left edge; the highlight keeps its full size.
            Transform.translate(
              offset: const Offset(-_actionInset, 0),
              child: Row(
                children: [
                  copy,
                  if (onFork != null)
                    IconButton(
                      tooltip: tr('从此处分叉', 'Fork from here'),
                      onPressed: onFork,
                      icon: const Icon(Icons.fork_right, size: 17),
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
        padding: const EdgeInsets.fromLTRB(14, 4, 6, 12),
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
              padding: const EdgeInsets.only(right: 8),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [...blocks(context, text, 14), ...states(context)],
              ),
            ),
          ],
        ),
      ),
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

class _MemoryContext extends StatefulWidget {
  final Future<List<dynamic>> Function()? read;
  final ValueChanged<String>? onSource;
  const _MemoryContext({this.read, this.onSource});
  @override
  State<_MemoryContext> createState() => _MemoryContextState();
}

class _MemoryContextState extends State<_MemoryContext> {
  List<dynamic> entries = [];
  String error = '';
  bool loading = false;
  Future<void> load(bool expanded) async {
    if (!expanded || loading || widget.read == null) return;
    setState(() {
      loading = true;
      entries = [];
      error = '';
    });
    try {
      final value = await widget.read!();
      if (mounted) setState(() => entries = value);
    } catch (_) {
      if (mounted) {
        setState(() => error = tr('无法读取记忆', 'Unable to load memories'));
      }
    } finally {
      if (mounted) setState(() => loading = false);
    }
  }

  @override
  Widget build(BuildContext context) => ExpansionTile(
    title: Text(
      tr('本轮引用的记忆', 'Memories referenced this turn'),
      style: TextStyle(fontSize: 12, color: HubColors.of(context).subtle),
    ),
    onExpansionChanged: load,
    children: [
      if (loading) const LinearProgressIndicator(),
      if (error.isNotEmpty) Text(error),
      for (final memory in entries)
        ListTile(
          dense: true,
          title: Text(memory['content'] ?? memory['title'] ?? ''),
          subtitle: memory['points'] == null ? null : Text(memory['points']),
          onTap: memory['source'] == null
              ? null
              : () => widget.onSource?.call(memory['source']),
        ),
      if (!loading && entries.isEmpty && error.isEmpty)
        Padding(
          padding: const EdgeInsets.all(12),
          child: Text(tr('记忆已失效或不可用', 'Memories are no longer available')),
        ),
    ],
  );
}

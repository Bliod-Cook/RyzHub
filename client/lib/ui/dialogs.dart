import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../controller.dart';
import '../l10n.dart';
import '../theme.dart';
import 'common.dart';
import 'icons.dart';

Future<String?> textDialog(
  BuildContext context,
  String title,
  String value,
) async {
  final input = TextEditingController(text: value);
  final result = await showDialog<String>(
    context: context,
    builder: (context) => AlertDialog(
      title: Text(title),
      content: SizedBox(
        width: 420,
        child: TextField(
          controller: input,
          decoration: InputDecoration(labelText: title),
          autofocus: true,
          onSubmitted: (v) => Navigator.pop(context, v),
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: Text(tr('取消', 'Cancel')),
        ),
        FilledButton(
          onPressed: () => Navigator.pop(context, input.text),
          child: Text(tr('保存', 'Save')),
        ),
      ],
    ),
  );
  // The dialog's route may still be animating its text field out.
  return result;
}

Future<bool> confirmDialog(
  BuildContext context, {
  required String title,
  required String message,
  required String confirm,
  bool danger = false,
}) async {
  final c = HubColors.of(context);
  return await showDialog<bool>(
        context: context,
        builder: (context) => AlertDialog(
          title: Text(title),
          content: SizedBox(width: 380, child: Text(message)),
          actions: [
            TextButton(
              onPressed: () => Navigator.pop(context, false),
              child: Text(tr('取消', 'Cancel')),
            ),
            FilledButton(
              style: danger
                  ? FilledButton.styleFrom(
                      backgroundColor: c.danger,
                      foregroundColor: Colors.white,
                    )
                  : null,
              onPressed: () => Navigator.pop(context, true),
              child: Text(confirm),
            ),
          ],
        ),
      ) ??
      false;
}

/// Pins or unpins a conversation, beside the header and sidebar row menus.
/// Reserved: disabled until the server advertises `conversation_pin`.
class PinButton extends StatelessWidget {
  final HubController hub;
  final Map<String, dynamic> conv;
  final RunAction run;

  /// Sidebar rows: a smaller glyph and no highlight of its own, since the
  /// row already highlights.
  final bool compact;
  const PinButton({
    super.key,
    required this.hub,
    required this.conv,
    required this.run,
    this.compact = false,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final pinned = conv['pinned'] == true;
    final size = compact ? 17.0 : 20.0;
    return IconButton(
      tooltip: !hub.canPin
          ? tr('置顶（即将支持）', 'Pin (coming soon)')
          : pinned
          ? tr('取消置顶', 'Unpin')
          : tr('置顶', 'Pin'),
      isSelected: pinned,
      onPressed: hub.canPin
          ? () => run(() => hub.pin(conv['id'], !pinned))
          : null,
      padding: compact ? EdgeInsets.zero : null,
      style: compact
          ? const ButtonStyle(
              overlayColor: WidgetStatePropertyAll(Colors.transparent),
            )
          : null,
      icon: Icon(
        Icons.push_pin_outlined,
        size: size,
        color: compact ? (hub.canPin ? c.muted : c.subtle) : null,
      ),
      selectedIcon: Icon(Icons.push_pin, size: size, color: c.accent),
    );
  }
}

/// Entries shared by the header `对话命令` menu and the sidebar row menu.
List<PopupMenuEntry<String>> conversationMenu(
  HubController hub,
  Map<String, dynamic> conv, {
  required bool header,
}) {
  final running = conv['id'] == hub.selected && hub.running;
  final archived = conv['state'] == 'archived';
  PopupMenuItem<String> item(
    String value,
    IconData icon,
    String label, {
    bool enabled = true,
    bool danger = false,
  }) => HubMenuItem(
    value: value,
    enabled: enabled,
    height: 38,
    child: Builder(
      builder: (context) {
        final c = HubColors.of(context);
        final color = danger ? c.danger : null;
        return Row(
          children: [
            Icon(icon, size: 17, color: color),
            const SizedBox(width: 12),
            Flexible(
              child: Text(label, style: TextStyle(color: color)),
            ),
          ],
        );
      },
    ),
  );
  final others = hub.activeProjects.where((p) => p['id'] != conv['project_id']);
  return [
    if (header && hub.supports('compact'))
      item(
        'compact',
        Icons.compress,
        tr('压缩上下文', 'Compact context'),
        enabled: !running,
      ),
    item('rename', Icons.edit_outlined, tr('重命名', 'Rename')),
    if (!archived)
      item(
        'archive',
        Icons.archive_outlined,
        tr('归档对话', 'Archive'),
        enabled: !running,
      )
    else if (conv['mode'] != 'work')
      item('unarchive', Icons.unarchive_outlined, tr('取消归档', 'Unarchive')),
    if (header)
      item('export', Icons.ios_share, tr('导出到剪贴板', 'Export to clipboard')),
    if (conv['project_id'] != null || others.isNotEmpty)
      const PopupMenuDivider(),
    if (conv['project_id'] != null)
      item(
        'project:',
        Icons.folder_off_outlined,
        tr('移出项目 · 记忆保留在原项目', 'Remove from project · memory stays there'),
        enabled: !running,
      ),
    for (final p in others)
      item(
        'project:${p['id']}',
        Icons.drive_file_move_outlined,
        tr('移入 ${p['name']}', 'Move to ${p['name']}'),
        enabled: !running,
      ),
    if (hub.canDelete) ...[
      const PopupMenuDivider(),
      item('delete', Icons.delete_outline, tr('删除', 'Delete'), danger: true),
    ],
    if (header) ...[
      const PopupMenuDivider(),
      item('refresh', Icons.refresh, tr('刷新命令能力', 'Refresh capabilities')),
    ],
  ];
}

Future<void> conversationAction(
  BuildContext context,
  HubController hub,
  Map<String, dynamic> conv,
  String value,
) async {
  final id = conv['id'] as String;
  switch (value) {
    case 'compact':
      await hub.compact();
    case 'rename':
      final title = await textDialog(
        context,
        tr('重命名对话', 'Rename conversation'),
        conv['title'] ?? '',
      );
      if (title != null && title.trim().isNotEmpty) {
        await hub.rename(id, title.trim());
      }
    case 'archive':
      await hub.archive(id);
    case 'unarchive':
      await hub.unarchive(id);
    case 'export':
      final data = await hub.exportConversation(id);
      await Clipboard.setData(
        ClipboardData(text: const JsonEncoder.withIndent('  ').convert(data)),
      );
    case 'delete':
      if (context.mounted &&
          await confirmDialog(
            context,
            title: tr('删除对话？', 'Delete conversation?'),
            message: tr(
              '将删除“${conv['title']}”。',
              'This will delete "${conv['title']}".',
            ),
            confirm: tr('删除', 'Delete'),
            danger: true,
          )) {
        await hub.deleteConversation(id);
      }
    case 'refresh':
      await hub.refreshCommands();
    default:
      if (value.startsWith('project:')) {
        final project = value.substring(8);
        await hub.moveProject(id, project.isEmpty ? null : project);
      }
  }
}

sealed class SearchResult {}

class SearchNewChat extends SearchResult {}

class SearchConversation extends SearchResult {
  final String id;
  SearchConversation(this.id);
}

class SearchProject extends SearchResult {
  final String id;
  SearchProject(this.id);
}

class SearchDialog extends StatefulWidget {
  final HubController hub;
  const SearchDialog({super.key, required this.hub});
  @override
  State<SearchDialog> createState() => _SearchDialogState();
}

class _SearchDialogState extends State<SearchDialog> {
  String query = '';
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final q = query.trim().toLowerCase();
    bool match(Object? v) => q.isEmpty || '$v'.toLowerCase().contains(q);
    final conversations = widget.hub.conversations
        .where((x) => match(x['title']))
        .toList();
    final projects = widget.hub.activeProjects
        .where((p) => match(p['name']))
        .toList();
    Widget row(
      IconData? icon,
      String label,
      SearchResult result, {
      String? badge,
      Widget? leading,
    }) => NavRow(
      icon: icon,
      leading: leading,
      label: label,
      onTap: () => Navigator.pop(context, result),
      trailing: badge == null
          ? null
          : Text(badge, style: TextStyle(fontSize: 12, color: c.subtle)),
    );
    return Dialog(
      alignment: const Alignment(0, -.5),
      insetPadding: const EdgeInsets.all(24),
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 620, maxHeight: 520),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(18, 10, 8, 6),
              child: Row(
                children: [
                  Expanded(
                    child: TextField(
                      autofocus: true,
                      onChanged: (v) => setState(() => query = v),
                      decoration: InputDecoration(
                        hintText: tr('搜索对话…', 'Search chats…'),
                        border: InputBorder.none,
                        enabledBorder: InputBorder.none,
                        focusedBorder: InputBorder.none,
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
            const Divider(),
            Flexible(
              child: ListView(
                shrinkWrap: true,
                padding: const EdgeInsets.symmetric(vertical: 8),
                children: [
                  row(
                    null,
                    tr('新建对话', 'New chat'),
                    SearchNewChat(),
                    leading: HubIcon(HubIcons.newChat, size: 18, color: c.text),
                  ),
                  if (projects.isNotEmpty) SectionLabel(tr('项目', 'Projects')),
                  for (final p in projects)
                    row(
                      Icons.folder_outlined,
                      p['name'],
                      SearchProject(p['id']),
                    ),
                  if (conversations.isNotEmpty) SectionLabel(tr('对话', 'Chats')),
                  for (final x in conversations)
                    row(
                      x['mode'] == 'work'
                          ? Icons.terminal
                          : Icons.chat_bubble_outline,
                      x['title'] ?? '',
                      SearchConversation(x['id']),
                      badge: x['state'] == 'archived'
                          ? tr('已归档', 'Archived')
                          : null,
                    ),
                  if (q.isNotEmpty && conversations.isEmpty && projects.isEmpty)
                    EmptyHint(tr('没有匹配的结果', 'No matches')),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class ProjectsDialog extends StatefulWidget {
  final HubController hub;
  final Map<String, dynamic>? initial;
  const ProjectsDialog({super.key, required this.hub, this.initial});
  @override
  State<ProjectsDialog> createState() => _ProjectsDialogState();
}

class _ProjectsDialogState extends State<ProjectsDialog> {
  final name = TextEditingController(),
      instructions = TextEditingController(),
      fileName = TextEditingController(),
      fileContent = TextEditingController();
  String memory = 'default', error = '';
  Map<String, dynamic>? selected;
  List<dynamic> files = [];
  bool busy = false;
  HubController get hub => widget.hub;
  @override
  void initState() {
    super.initState();
    if (widget.initial != null) run(() => choose(widget.initial));
  }

  @override
  void dispose() {
    name.dispose();
    instructions.dispose();
    fileName.dispose();
    fileContent.dispose();
    super.dispose();
  }

  Future<void> run(Future<void> Function() f) async {
    setState(() {
      busy = true;
      error = '';
    });
    try {
      await f();
      await hub.refreshProjects();
    } catch (e) {
      error = errorText(e);
    }
    if (mounted) setState(() => busy = false);
  }

  Future<void> choose(Map<String, dynamic>? p) async {
    setState(() {
      selected = p;
      name.text = p?['name'] ?? '';
      instructions.text = p?['instructions'] ?? '';
      memory = p?['memory_mode'] ?? 'default';
      files = [];
    });
    if (p != null && p['state'] == 'active') {
      files = await hub.request('/orgs/${hub.org}/projects/${p['id']}/files');
      if (mounted) setState(() {});
    }
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return AlertDialog(
      title: Text(tr('项目', 'Projects')),
      content: SizedBox(
        width: 620,
        child: SingleChildScrollView(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Wrap(
                spacing: 8,
                runSpacing: 8,
                children: [
                  ActionChip(
                    avatar: const Icon(Icons.add, size: 16),
                    label: Text(tr('新项目', 'New project')),
                    onPressed: () => choose(null),
                  ),
                  for (final p in hub.projects)
                    InputChip(
                      label: Text(
                        '${p['name']}${p['state'] == 'deletion_pending' ? tr(' · 可恢复', ' · restorable') : ''}',
                      ),
                      selected: selected?['id'] == p['id'],
                      onSelected: (_) => run(() => choose(p)),
                    ),
                ],
              ),
              const SizedBox(height: 16),
              TextField(
                controller: name,
                decoration: InputDecoration(
                  labelText: tr('项目名称', 'Project name'),
                ),
              ),
              const SizedBox(height: 12),
              TextField(
                controller: instructions,
                minLines: 3,
                maxLines: 6,
                decoration: InputDecoration(
                  labelText: tr('项目指令', 'Project instructions'),
                ),
              ),
              const SizedBox(height: 12),
              DropdownButtonFormField<String>(
                key: ValueKey('${selected?['id']}:$memory'),
                initialValue: memory,
                decoration: InputDecoration(
                  labelText: tr(
                    '记忆边界（创建后不可更改）',
                    'Memory boundary (fixed after creation)',
                  ),
                ),
                items: [
                  DropdownMenuItem(
                    value: 'default',
                    child: Text(tr('默认', 'Default')),
                  ),
                  DropdownMenuItem(
                    value: 'project_only',
                    child: Text(tr('仅此项目', 'This project only')),
                  ),
                ],
                onChanged: selected == null
                    ? (v) => setState(() => memory = v!)
                    : null,
              ),
              const SizedBox(height: 8),
              Text(
                tr(
                  '私有项目。指令与文件在下一轮生效。',
                  'Private project. Instructions and files apply from the next turn.',
                ),
                style: TextStyle(fontSize: 12, color: c.subtle),
              ),
              const SizedBox(height: 12),
              Wrap(
                spacing: 8,
                children: [
                  if (selected?['state'] != 'deletion_pending')
                    FilledButton(
                      onPressed: busy
                          ? null
                          : () => run(() async {
                              final data = {
                                'name': name.text,
                                'instructions': instructions.text,
                                'memory_mode': memory,
                                'defaults':
                                    selected?['defaults'] ??
                                    {
                                      'mode': hub.mode,
                                      if (hub.mode == 'work')
                                        'harness': hub.harness,
                                      // The server derives the wire protocol.
                                      if (hub.isDraft && hub.draftModel != null)
                                        'model': hub.draftModel,
                                    },
                              };
                              if (selected == null) {
                                final p = await hub.request(
                                  '/orgs/${hub.org}/projects',
                                  body: data,
                                );
                                await choose(Map<String, dynamic>.from(p));
                              } else {
                                await hub.request(
                                  '/orgs/${hub.org}/projects/${selected!['id']}',
                                  body: data,
                                  method: 'PATCH',
                                );
                              }
                            }),
                      child: Text(tr('保存项目', 'Save project')),
                    ),
                  if (selected != null)
                    TextButton(
                      onPressed: busy
                          ? null
                          : () => run(() async {
                              final deleted =
                                  selected!['state'] == 'deletion_pending';
                              await hub.request(
                                '/orgs/${hub.org}/projects/${selected!['id']}${deleted ? '/restore' : ''}',
                                body: deleted ? {} : null,
                                method: deleted ? 'POST' : 'DELETE',
                              );
                              await hub.refreshConversations();
                              await choose(null);
                            }),
                      child: Text(
                        selected!['state'] == 'deletion_pending'
                            ? tr('恢复项目', 'Restore project')
                            : tr(
                                '删除项目（30 天可恢复）',
                                'Delete project (restorable for 30 days)',
                              ),
                      ),
                    ),
                ],
              ),
              if (selected != null && selected!['state'] == 'active') ...[
                const SizedBox(height: 14),
                const Divider(),
                const SizedBox(height: 14),
                Text(tr('知识文件 · UTF-8 文本', 'Knowledge files · UTF-8 text')),
                for (final f in files)
                  ListTile(
                    dense: true,
                    title: Text(f['name']),
                    onTap: () => setState(() {
                      fileName.text = f['name'];
                      fileContent.text = f['content'];
                    }),
                    trailing: IconButton(
                      tooltip: tr('删除文件', 'Delete file'),
                      icon: const Icon(Icons.delete_outline),
                      onPressed: () => run(() async {
                        await hub.request(
                          '/orgs/${hub.org}/projects/${selected!['id']}/files/${f['id']}',
                          method: 'DELETE',
                        );
                        await choose(selected);
                      }),
                    ),
                  ),
                const SizedBox(height: 8),
                TextField(
                  controller: fileName,
                  decoration: InputDecoration(
                    labelText: tr('文件名', 'File name'),
                  ),
                ),
                const SizedBox(height: 8),
                TextField(
                  controller: fileContent,
                  minLines: 3,
                  maxLines: 8,
                  decoration: InputDecoration(
                    labelText: tr('文件内容', 'File content'),
                  ),
                ),
                const SizedBox(height: 8),
                Align(
                  alignment: Alignment.centerLeft,
                  child: OutlinedButton(
                    onPressed: busy
                        ? null
                        : () => run(() async {
                            await hub.request(
                              '/orgs/${hub.org}/projects/${selected!['id']}/files',
                              body: {
                                'name': fileName.text,
                                'content': fileContent.text,
                              },
                            );
                            await choose(selected);
                          }),
                    child: Text(tr('保存知识文件', 'Save knowledge file')),
                  ),
                ),
              ],
              if (error.isNotEmpty)
                Padding(
                  padding: const EdgeInsets.only(top: 8),
                  child: Text(error, style: TextStyle(color: c.danger)),
                ),
            ],
          ),
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: Text(tr('完成', 'Done')),
        ),
      ],
    );
  }
}

import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'controller.dart';

typedef RunAction = Future<void> Function(Future<void> Function());

class CommandBar extends StatelessWidget {
  final HubController hub;
  final RunAction action;
  const CommandBar({super.key, required this.hub, required this.action});
  Widget picker(
    String label,
    String key,
    List<String> values,
    Map<String, dynamic> settings,
  ) => SizedBox(
    width: key == 'model' ? 225 : 150,
    child: DropdownButtonFormField<String>(
      key: ValueKey('$key:${settings[key]}'),
      initialValue: values.contains(settings[key]) ? settings[key] : null,
      isExpanded: true,
      decoration: InputDecoration(labelText: label, isDense: true),
      items: values
          .map(
            (v) => DropdownMenuItem(
              value: v,
              child: Text(
                {
                      'ask': '每次询问',
                      'allow_edits': '自动允许编辑',
                      'full_auto': '全部自动',
                    }[v] ??
                    v,
                overflow: TextOverflow.ellipsis,
              ),
            ),
          )
          .toList(),
      onChanged: (v) => action(() => hub.configure({key: v})),
    ),
  );
  @override
  Widget build(BuildContext context) {
    if (hub.commandCatalog.isEmpty || (!hub.realChat && !hub.realWork)) {
      return const SizedBox.shrink();
    }
    final settings = hub.settings;
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 24, vertical: 8),
      child: Wrap(
        spacing: 8,
        runSpacing: 8,
        crossAxisAlignment: WrapCrossAlignment.center,
        children: [
          picker(
            '模型',
            'model',
            (hub.commandCatalog['models'] as List? ?? []).cast<String>(),
            settings,
          ),
          if (hub.supports('thought_level'))
            picker(
              '推理强度',
              'reasoning',
              (hub.commandCatalog['reasoning_levels'] as List? ?? ['medium'])
                  .cast<String>(),
              settings,
            ),
          picker(
            '审批模式',
            'approval',
            (hub.commandCatalog['approval_modes'] as List? ?? ['ask'])
                .cast<String>(),
            settings,
          ),
          if (hub.supports('plan_mode'))
            FilterChip(
              label: const Text('计划模式'),
              selected: settings['plan'] == true,
              onSelected: (v) => action(() => hub.configure({'plan': v})),
            ),
          const Text(
            '设置在下一轮生效',
            style: TextStyle(fontSize: 12, color: Colors.grey),
          ),
          if (settings['harness'] == 'claude')
            const Text('补充指令排入下一轮', style: TextStyle(fontSize: 12)),
          PopupMenuButton<String>(
            tooltip: '对话命令',
            onSelected: (value) => action(() async {
              if (value == 'compact') await hub.compact();
              if (value == 'export') {
                final data = await hub.request(
                  '/orgs/${hub.org}/conversations/${hub.selected}/export',
                );
                await Clipboard.setData(
                  ClipboardData(
                    text: const JsonEncoder.withIndent('  ').convert(data),
                  ),
                );
              }
              if (value == 'rename' && context.mounted) {
                final title = await textDialog(
                  context,
                  '重命名对话',
                  hub.selectedConversation['title'] ?? '',
                );
                if (title != null) {
                  await hub.request(
                    '/orgs/${hub.org}/conversations/${hub.selected}/settings',
                    method: 'PATCH',
                    body: {'title': title},
                  );
                  await hub.refreshConversations();
                }
              }
              if (value == 'archive') {
                if (hub.realWork) {
                  await hub.workspaceOperation('archive');
                } else {
                  await hub.request(
                    '/orgs/${hub.org}/conversations/${hub.selected}/settings',
                    method: 'PATCH',
                    body: {'archive': true},
                  );
                  await hub.refreshConversations();
                }
              }
              if (value == 'refresh') await hub.refreshCommands();
              if (value.startsWith('project:')) {
                await hub.moveProject(
                  value.substring(8).isEmpty ? null : value.substring(8),
                );
              }
              if (value.startsWith('harness:')) {
                await hub.send('/${value.substring(8)}');
              }
            }),
            itemBuilder: (_) => [
              if (hub.supports('compact'))
                PopupMenuItem(
                  value: 'compact',
                  enabled: !hub.running,
                  child: const Text('压缩上下文'),
                ),
              const PopupMenuItem(value: 'rename', child: Text('重命名')),
              PopupMenuItem(
                value: 'archive',
                enabled: !hub.running,
                child: const Text('归档对话'),
              ),
              const PopupMenuItem(value: 'export', child: Text('导出到剪贴板')),
              const PopupMenuDivider(),
              PopupMenuItem(
                value: 'project:',
                enabled: !hub.running,
                child: const Text('移出项目 · 记忆保留在原项目'),
              ),
              for (final p in hub.projects.where((p) => p['state'] == 'active'))
                PopupMenuItem(
                  value: 'project:${p['id']}',
                  enabled: !hub.running,
                  child: Text('移入 ${p['name']}'),
                ),
              if ((hub.commandCatalog['harness_commands'] as List? ?? [])
                  .isNotEmpty)
                const PopupMenuDivider(),
              for (final c
                  in (hub.commandCatalog['harness_commands'] as List? ?? [])
                      .where((c) => c is Map && c['name'] is String))
                PopupMenuItem(
                  value: 'harness:${c['name']}',
                  enabled: !hub.running,
                  child: Text('Harness /${c['name']}'),
                ),
              const PopupMenuItem(value: 'refresh', child: Text('刷新命令能力')),
            ],
          ),
        ],
      ),
    );
  }
}

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
      content: TextField(
        controller: input,
        decoration: InputDecoration(labelText: title),
        autofocus: true,
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: const Text('取消'),
        ),
        FilledButton(
          onPressed: () => Navigator.pop(context, input.text),
          child: const Text('保存'),
        ),
      ],
    ),
  );
  // The dialog's route may still be animating its text field out.
  return result;
}

class ProjectsDialog extends StatefulWidget {
  final HubController hub;
  const ProjectsDialog({super.key, required this.hub});
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
      error = '$e';
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
  Widget build(BuildContext context) => AlertDialog(
    title: const Text('项目'),
    content: SizedBox(
      width: 620,
      child: SingleChildScrollView(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Wrap(
              spacing: 8,
              children: [
                ActionChip(
                  label: const Text('新项目'),
                  onPressed: () => choose(null),
                ),
                for (final p in hub.projects)
                  InputChip(
                    label: Text(
                      '${p['name']}${p['state'] == 'deletion_pending' ? ' · 可恢复' : ''}',
                    ),
                    selected: selected?['id'] == p['id'],
                    onSelected: (_) => run(() => choose(p)),
                  ),
              ],
            ),
            const SizedBox(height: 16),
            TextField(
              controller: name,
              decoration: const InputDecoration(labelText: '项目名称'),
            ),
            const SizedBox(height: 12),
            TextField(
              controller: instructions,
              minLines: 3,
              maxLines: 6,
              decoration: const InputDecoration(labelText: '项目指令'),
            ),
            const SizedBox(height: 12),
            DropdownButtonFormField<String>(
              key: ValueKey('${selected?['id']}:$memory'),
              initialValue: memory,
              decoration: const InputDecoration(labelText: '记忆边界（创建后不可更改）'),
              items: const [
                DropdownMenuItem(value: 'default', child: Text('默认')),
                DropdownMenuItem(value: 'project_only', child: Text('仅此项目')),
              ],
              onChanged: selected == null
                  ? (v) => setState(() => memory = v!)
                  : null,
            ),
            const SizedBox(height: 8),
            const Text('私有项目。指令与文件在下一轮生效。', style: TextStyle(fontSize: 12)),
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
                                    'harness': hub.mode == 'work'
                                        ? hub.harness
                                        : 'chat',
                                    'protocol': hub.protocol,
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
                    child: const Text('保存项目'),
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
                          ? '恢复项目'
                          : '删除项目（30 天可恢复）',
                    ),
                  ),
              ],
            ),
            if (selected != null && selected!['state'] == 'active') ...[
              const Divider(height: 30),
              const Text('知识文件 · UTF-8 文本'),
              for (final f in files)
                ListTile(
                  dense: true,
                  title: Text(f['name']),
                  onTap: () => setState(() {
                    fileName.text = f['name'];
                    fileContent.text = f['content'];
                  }),
                  trailing: IconButton(
                    tooltip: '删除文件',
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
              TextField(
                controller: fileName,
                decoration: const InputDecoration(labelText: '文件名'),
              ),
              const SizedBox(height: 8),
              TextField(
                controller: fileContent,
                minLines: 3,
                maxLines: 8,
                decoration: const InputDecoration(labelText: '文件内容'),
              ),
              const SizedBox(height: 8),
              OutlinedButton(
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
                child: const Text('保存知识文件'),
              ),
            ],
            if (error.isNotEmpty)
              Text(error, style: const TextStyle(color: Colors.red)),
          ],
        ),
      ),
    ),
    actions: [
      TextButton(
        onPressed: () => Navigator.pop(context),
        child: const Text('完成'),
      ),
    ],
  );
}

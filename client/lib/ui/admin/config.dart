import 'dart:convert';

import 'package:flutter/material.dart';

import '../../controller.dart';
import '../../l10n.dart';
import '../../theme.dart';
import '../dialogs.dart';
import '../sectioned.dart';
import 'config_fields.dart';
import 'fields.dart';
import 'services.dart';

/// Retention settings that delete data once shortened, with how many seconds
/// one of their units is. Zero or null keeps data forever.
const _retention = {
  'events_retention_days': 86400,
  'retention.usage_days': 86400,
  'retention.audit_days': 86400,
  'work.checkpoint_retention_days': 86400,
  'policy.archive_retention_secs': 1,
};

const _embedding = {
  'search.embedding_provider',
  'search.embedding_url',
  'search.embedding_model',
};

/// One section of the instance configuration, saved on its own.
class ConfigSection extends StatefulWidget {
  final HubController hub;
  final String section;
  const ConfigSection({super.key, required this.hub, required this.section});
  @override
  State<ConfigSection> createState() => _ConfigSectionState();
}

class _ConfigSectionState extends State<ConfigSection> {
  Map<String, dynamic>? data;
  final changed = <String, Object?>{};
  String error = '', notice = '';
  bool busy = false, conflict = false;

  /// Bumped whenever the values shown are replaced, so controls start over.
  int generation = 0;

  @override
  void initState() {
    super.initState();
    load();
  }

  Future<void> load() async {
    setState(() {
      error = '';
      conflict = false;
    });
    try {
      final loaded = Map<String, dynamic>.from(
        await widget.hub.admin('/admin/config'),
      );
      if (!mounted) return;
      setState(() {
        data = loaded;
        changed.clear();
        generation++;
      });
    } catch (e) {
      if (mounted) setState(() => error = errorText(e));
    }
  }

  Map<String, dynamic> get config =>
      Map<String, dynamic>.from(data?['config'] ?? {});

  Object? value(String path) =>
      changed.containsKey(path) ? changed[path] : valueAt(config, path);

  void set(String path, Object? v) => setState(() {
    if (jsonEncode(v) == jsonEncode(valueAt(config, path))) {
      changed.remove(path);
    } else {
      changed[path] = v;
    }
  });

  /// How a change at [path] applies and whether it may be made here.
  Map field(String path) {
    final fields = data?['fields'] as Map? ?? {};
    return fields[path] as Map? ??
        fields.entries
            .where((e) => '${e.key}'.startsWith('$path.'))
            .map((e) => e.value as Map)
            .firstOrNull ??
        const {'apply': 'live', 'editable': false};
  }

  /// The confirmations the changes need; false once the admin backs out, and
  /// whether to rebuild the search index after.
  Future<(bool, bool)> confirmed() async {
    final cut = <int>[];
    for (final MapEntry(:key, value: unit) in _retention.entries) {
      if (!changed.containsKey(key)) continue;
      final old = valueAt(config, key) as num? ?? 0;
      final now = changed[key] as num? ?? 0;
      if (now > 0 && (old == 0 || now < old)) cut.add((now * unit).toInt());
    }
    if (cut.isNotEmpty) {
      final shortest = cut.reduce((a, b) => a < b ? a : b);
      final day = DateTime.now().subtract(Duration(seconds: shortest));
      final date =
          '${day.year}-${'${day.month}'.padLeft(2, '0')}-${'${day.day}'.padLeft(2, '0')}';
      if (!mounted ||
          !await confirmDialog(
            context,
            title: tr('缩短保留期？', 'Shorten retention?'),
            message: tr(
              '早于 $date 的数据会被删除。',
              'Data older than $date will be deleted.',
            ),
            confirm: tr('保存', 'Save'),
            danger: true,
          )) {
        return (false, false);
      }
    }
    if (changed.containsKey('harness.acp')) {
      String? version(List list, Object name) =>
          list.where((a) => a['name'] == name).firstOrNull?['version']
              as String?;
      final before = valueAt(config, 'harness.acp') as List? ?? [];
      final after = changed['harness.acp'] as List? ?? [];
      final bumped = after.any((a) {
        final old = version(before, a['name']);
        return old != null && old != a['version'];
      });
      if (bumped &&
          (!mounted ||
              !await confirmDialog(
                context,
                title: tr('修改 Harness 版本？', 'Change the harness version?'),
                message: tr(
                  '旧版本留下的检查点将无法恢复。',
                  'Checkpoints taken with the old version will not restore.',
                ),
                confirm: tr('保存', 'Save'),
                danger: true,
              ))) {
        return (false, false);
      }
    }
    if (changed.keys.any(_embedding.contains)) {
      if (!mounted) return (false, false);
      final choice = await showDialog<String>(
        context: context,
        builder: (context) => AlertDialog(
          title: Text(tr('需要重建搜索索引', 'The search index needs rebuilding')),
          content: SizedBox(
            width: 380,
            child: Text(
              tr(
                '向量换了来源，已有的索引不再匹配；重建前向量检索查不到旧内容。',
                'Vectors from another source do not match the index; vector search misses older content until it is rebuilt.',
              ),
            ),
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.pop(context),
              child: Text(tr('取消', 'Cancel')),
            ),
            TextButton(
              onPressed: () => Navigator.pop(context, 'save'),
              child: Text(tr('仅保存', 'Save only')),
            ),
            FilledButton(
              onPressed: () => Navigator.pop(context, 'reindex'),
              child: Text(tr('保存并重建', 'Save and rebuild')),
            ),
          ],
        ),
      );
      if (choice == null) return (false, false);
      return (true, choice == 'reindex');
    }
    return (true, false);
  }

  Future<void> save() async {
    final (go, reindex) = await confirmed();
    if (!go) return;
    setState(() {
      busy = true;
      error = '';
      notice = '';
    });
    try {
      final saved = Map<String, dynamic>.from(
        await widget.hub.admin(
          '/admin/config/${widget.section}',
          method: 'PUT',
          body: {'version': data!['version'], 'values': changed},
        ),
      );
      var queued = '';
      if (reindex) {
        final result = await widget.hub.admin(
          '/admin/search/reindex',
          body: {},
        );
        queued = tr(
          '已排队重建 ${result['queued']} 项。',
          '${result['queued']} items queued for indexing.',
        );
      }
      if (!mounted) return;
      setState(() {
        data = saved;
        changed.clear();
        generation++;
        notice = queued;
      });
    } catch (e) {
      if (mounted) {
        setState(() {
          error = errorText(e);
          conflict = '$e'.contains('CONFIG_CHANGED');
        });
      }
    }
    if (mounted) setState(() => busy = false);
  }

  Widget banner(
    BuildContext context,
    Color color,
    String text, [
    Widget? action,
  ]) => Container(
    margin: const EdgeInsets.only(bottom: Space.edge),
    padding: const EdgeInsets.all(Space.inset),
    decoration: BoxDecoration(
      borderRadius: BorderRadius.circular(Radii.field),
      border: Border.all(color: color),
    ),
    child: Row(
      children: [
        Expanded(child: SelectableText(text)),
        ?action,
      ],
    ),
  );

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final data = this.data;
    if (data == null) {
      return error.isEmpty
          ? const Center(child: CircularProgressIndicator(strokeWidth: 2))
          : Column(
              children: [
                Text(error, style: TextStyle(color: c.danger)),
                TextButton(onPressed: load, child: Text(tr('重试', 'Retry'))),
              ],
            );
    }
    final editable = data['editable'] == true;
    final env = {
      for (final MapEntry(:key, :value) in (data['env'] as Map? ?? {}).entries)
        '$key': value == true,
    };
    final pending = strings(data['pending_restart']);
    final reloadError = data['reload_error'];
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (reloadError != null)
          banner(
            context,
            c.danger,
            tr(
              '配置文件有误，仍在使用上次的配置：$reloadError',
              'The configuration file is invalid; the last good one stays in use: $reloadError',
            ),
          ),
        if (pending.isNotEmpty)
          banner(
            context,
            c.warning,
            tr(
              '重启进程后生效：${pending.join(', ')}',
              'Take effect after a restart: ${pending.join(', ')}',
            ),
            TextButton(
              onPressed: () async {
                try {
                  await restartProcess(context, widget.hub);
                  await load();
                } catch (e) {
                  if (mounted) setState(() => error = errorText(e));
                }
              },
              child: Text(tr('重启进程', 'Restart process')),
            ),
          ),
        if (!editable)
          banner(
            context,
            c.border,
            tr(
              '服务器没有可写的配置文件（RYZHUB_CONFIG），这里只能查看。',
              'The server has no writable configuration file (RYZHUB_CONFIG); this is read-only.',
            ),
          ),
        if (widget.section == 'retention')
          Padding(
            padding: const EdgeInsets.only(bottom: Space.gap),
            child: Text(
              tr(
                '保留期的修改最多一天后生效。',
                'Retention changes take effect within a day.',
              ),
              style: TextStyle(fontSize: FontSizes.caption, color: c.subtle),
            ),
          ),
        SettingsCard(
          children: [
            for (final f in fieldsOf(widget.section))
              KeyedSubtree(
                key: ValueKey('${f.path}@$generation'),
                child: _FieldRow(
                  field: f,
                  apply: '${field(f.path)['apply']}',
                  pending: pending.any(
                    (p) =>
                        p == f.path ||
                        p.startsWith('${f.path}.') ||
                        f.path.startsWith('$p.'),
                  ),
                  input: FieldInput(
                    value(f.path),
                    config,
                    env,
                    editable && field(f.path)['editable'] == true && !busy
                        ? (v) => set(f.path, v)
                        : null,
                  ),
                ),
              ),
          ],
        ),
        if (notice.isNotEmpty) Text(notice, style: TextStyle(color: c.success)),
        if (error.isNotEmpty)
          Padding(
            padding: const EdgeInsets.only(bottom: Space.gap),
            child: Row(
              children: [
                Expanded(
                  child: SelectableText(
                    error,
                    style: TextStyle(color: c.danger),
                  ),
                ),
                if (conflict)
                  TextButton(
                    onPressed: load,
                    child: Text(tr('重新加载', 'Reload')),
                  ),
              ],
            ),
          ),
        if (changed.isNotEmpty)
          Container(
            padding: const EdgeInsets.symmetric(
              horizontal: Space.inset,
              vertical: Space.gap,
            ),
            decoration: BoxDecoration(
              color: c.bubble,
              borderRadius: BorderRadius.circular(Radii.field),
            ),
            child: Row(
              children: [
                Expanded(child: Text(tr('有未保存的修改', 'Unsaved changes'))),
                TextButton(
                  onPressed: busy
                      ? null
                      : () => setState(() {
                          changed.clear();
                          generation++;
                          error = '';
                        }),
                  child: Text(tr('放弃', 'Discard')),
                ),
                const SizedBox(width: Space.gap),
                FilledButton(
                  onPressed: busy ? null : save,
                  child: Text(tr('保存', 'Save')),
                ),
              ],
            ),
          ),
      ],
    );
  }
}

class _FieldRow extends StatelessWidget {
  final Field field;
  final String apply;
  final bool pending;
  final FieldInput input;
  const _FieldRow({
    required this.field,
    required this.apply,
    required this.pending,
    required this.input,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final label = Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Wrap(
          spacing: Space.gap,
          crossAxisAlignment: WrapCrossAlignment.center,
          children: [
            Text(
              field.label(),
              style: TextStyle(fontSize: FontSizes.body, color: c.text),
            ),
            ApplyBadge(apply, pending: pending),
            if (input.onChanged == null)
              Tooltip(
                message: tr('只能在配置文件里修改', 'Set in the configuration file'),
                child: Icon(Icons.lock_outline, size: 14, color: c.subtle),
              ),
          ],
        ),
        if (field.hint != null)
          Text(
            field.hint!(),
            style: TextStyle(fontSize: FontSizes.caption, color: c.subtle),
          ),
      ],
    );
    final control = field.control(context, input);
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 12),
      child: field.wide
          ? Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                label,
                const SizedBox(height: Space.gap),
                control,
              ],
            )
          : Row(
              children: [
                Expanded(child: label),
                const SizedBox(width: 16),
                Flexible(child: control),
              ],
            ),
    );
  }
}

import 'dart:async';

import 'package:flutter/material.dart';

import '../../controller.dart';
import '../../l10n.dart';
import '../../theme.dart';
import '../common.dart';
import '../dialogs.dart';
import '../sectioned.dart';
import 'admin.dart';

/// Restarts the server process the panel talks to, once the admin confirms,
/// and waits for it to answer again.
Future<void> restartProcess(BuildContext context, HubController hub) async {
  if (!await confirmDialog(
    context,
    title: tr('重启服务进程？', 'Restart the server process?'),
    message: tr(
      '进行中的流会中断，与 docker compose restart 相同。.env 的修改需要在宿主机执行 docker compose up -d。',
      'Streams in progress are cut, as with docker compose restart. Changes to .env need docker compose up -d on the host.',
    ),
    confirm: tr('重启', 'Restart'),
    danger: true,
  )) {
    return;
  }
  await hub.admin('/admin/process/restart', body: {});
  if (!context.mounted) return;
  final navigator = Navigator.of(context, rootNavigator: true);
  unawaited(
    showDialog<void>(
      context: context,
      barrierDismissible: false,
      builder: (context) => AlertDialog(
        content: Row(
          children: [
            const SizedBox(
              width: 18,
              height: 18,
              child: CircularProgressIndicator(strokeWidth: 2),
            ),
            const SizedBox(width: Space.edge),
            Text(tr('重连中', 'Reconnecting')),
          ],
        ),
      ),
    ),
  );
  final back = await _serverBack(hub);
  navigator.pop();
  if (!back) {
    throw Exception(
      tr('服务器两分钟内没有恢复。', 'The server did not come back within two minutes.'),
    );
  }
}

/// Whether `/healthz` answers again within two minutes. The old process may
/// still answer for a moment, so an answer counts once one failed, or after
/// ten seconds.
Future<bool> _serverBack(HubController hub) async {
  final started = DateTime.now();
  var down = false;
  while (DateTime.now().difference(started) < const Duration(minutes: 2)) {
    await Future<void>.delayed(const Duration(seconds: 1));
    try {
      await hub.request('/healthz', timeout: const Duration(seconds: 3));
      if (down ||
          DateTime.now().difference(started) > const Duration(seconds: 10)) {
        return true;
      }
    } catch (_) {
      down = true;
    }
  }
  return false;
}

/// This process's background loops and listeners, and the execution nodes.
class Services extends StatefulWidget {
  final HubController hub;
  const Services({super.key, required this.hub});
  @override
  State<Services> createState() => _ServicesState();
}

class _ServicesState extends State<Services> {
  final loader = GlobalKey<LoaderState<Map<String, dynamic>>>();
  String error = '';

  Future<void> run(Future<void> Function() task) async {
    setState(() => error = '');
    try {
      await task();
    } catch (e) {
      if (mounted) setState(() => error = errorText(e));
    }
    loader.currentState?.reload();
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Loader<Map<String, dynamic>>(
      key: loader,
      load: () async => Map<String, dynamic>.from(
        await widget.hub.admin('/admin/components'),
      ),
      builder: (context, data, reload) {
        final process = data['process'] as Map? ?? {};
        final components = (data['components'] as List? ?? []).cast<Map>();
        final nodes = (data['nodes'] as List? ?? []).cast<Map>();
        final muted = TextStyle(fontSize: FontSizes.label, color: c.muted);
        return Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            SettingsCard(
              title: tr('本进程', 'This process'),
              children: [
                SettingRow(
                  title: '${process['role']} · ${process['host']}',
                  subtitle: [
                    'pid ${process['pid']}',
                    tr(
                      '启动于 ${formatTime(process['started_at_ms'] as num?)}',
                      'Started ${formatTime(process['started_at_ms'] as num?)}',
                    ),
                    tr(
                      '版本 ${process['version']}',
                      'Version ${process['version']}',
                    ),
                    if (process['config_version'] case final String v)
                      tr(
                        '配置 ${v.substring(0, v.length.clamp(0, 8))}',
                        'Config ${v.substring(0, v.length.clamp(0, 8))}',
                      ),
                  ].join(' · '),
                  trailing: OutlinedButton(
                    style: OutlinedButton.styleFrom(foregroundColor: c.danger),
                    onPressed: () =>
                        run(() => restartProcess(context, widget.hub)),
                    child: Text(tr('重启进程', 'Restart process')),
                  ),
                ),
              ],
            ),
            SettingsCard(
              title: tr('组件', 'Components'),
              children: [
                for (final component in components)
                  SettingRow(
                    title: '${component['name']}',
                    subtitle: [
                      component['state'] == 'backoff'
                          ? tr('退避中', 'Backing off')
                          : tr('运行中', 'Running'),
                      if (component['started_at_ms'] != null)
                        tr(
                          '启动于 ${formatTime(component['started_at_ms'] as num?)}',
                          'Started ${formatTime(component['started_at_ms'] as num?)}',
                        ),
                      if ((component['restarts'] as num? ?? 0) > 0)
                        tr(
                          '重启 ${component['restarts']} 次',
                          'Restarted ${component['restarts']} times',
                        ),
                      if (component['error'] != null) '${component['error']}',
                    ].join(' · '),
                    trailing: component['restartable'] == true
                        ? TextButton(
                            onPressed: () => run(
                              () => widget.hub.admin(
                                '/admin/components/${Uri.encodeComponent('${component['name']}')}/restart',
                                body: {},
                              ),
                            ),
                            child: Text(tr('重启', 'Restart')),
                          )
                        : null,
                  ),
              ],
            ),
            SettingsCard(
              title: tr('执行节点', 'Execution nodes'),
              children: [
                if (nodes.isEmpty)
                  SettingRow(title: tr('没有执行节点', 'No execution nodes')),
                for (final node in nodes)
                  SettingRow(
                    title: '${node['name']}',
                    subtitle: tr(
                      '租约到 ${formatTime(node['lease_until_ms'] as num?)}',
                      'Lease until ${formatTime(node['lease_until_ms'] as num?)}',
                    ),
                    trailing: Text(
                      '${node['state']} · ${node['live']} / ${(node['capacity'] as num? ?? 0) == 0 ? tr('不限', 'unlimited') : node['capacity']}',
                      style: muted,
                    ),
                  ),
              ],
            ),
            Row(
              children: [
                TextButton.icon(
                  onPressed: reload,
                  icon: const Icon(Icons.refresh, size: 16),
                  label: Text(tr('刷新', 'Refresh')),
                ),
                if (error.isNotEmpty)
                  Expanded(
                    child: Text(error, style: TextStyle(color: c.danger)),
                  ),
              ],
            ),
          ],
        );
      },
    );
  }
}

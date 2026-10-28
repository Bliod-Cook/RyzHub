import 'package:flutter/material.dart';

import '../../controller.dart';
import '../../l10n.dart';
import '../sectioned.dart';
import 'config.dart';
import 'config_fields.dart';
import 'overview.dart';
import 'services.dart';
import 'users.dart';

/// The instance admin panel. It closes once the server stops taking the
/// user as an admin.
class AdminPage extends StatefulWidget {
  final HubController hub;
  const AdminPage({super.key, required this.hub});
  @override
  State<AdminPage> createState() => _AdminPageState();
}

class _AdminPageState extends State<AdminPage> {
  bool leaving = false;

  @override
  Widget build(BuildContext context) => ListenableBuilder(
    listenable: widget.hub,
    builder: (context, _) {
      if (!widget.hub.isInstanceAdmin && !leaving) {
        leaving = true;
        WidgetsBinding.instance.addPostFrameCallback((_) {
          if (mounted) Navigator.of(context).maybePop();
        });
      }
      return SectionedPage(
        back: tr('返回设置', 'Back to settings'),
        home: 'overview',
        groups: [
          ('main', () => ''),
          ('config', () => tr('配置', 'Configuration')),
        ],
        sections: [
          PageSection(
            'overview',
            'main',
            Icons.insights_outlined,
            () => tr('概览', 'Overview'),
            ['统计', 'stats', 'token', '图表', 'chart'],
            width: 1100,
          ),
          PageSection(
            'users',
            'main',
            Icons.people_outline,
            () => tr('用户', 'Users'),
            ['用户', 'user', '禁用', 'disable', '删除', 'delete', 'email'],
            width: 1100,
          ),
          PageSection(
            'services',
            'main',
            Icons.memory_outlined,
            () => tr('服务', 'Services'),
            ['重启', 'restart', '组件', 'component', '节点', 'node'],
            width: 760,
          ),
          for (final s in configSections)
            PageSection(
              s.id,
              'config',
              s.icon,
              s.title,
              s.keywords,
              width: 760,
            ),
        ],
        content: (context, s) => switch (s.id) {
          'overview' => Overview(hub: widget.hub),
          'users' => Users(hub: widget.hub),
          'services' => Services(hub: widget.hub),
          _ => ConfigSection(hub: widget.hub, section: s.id),
        },
      );
    },
  );
}

/// [value] bytes in the largest unit that keeps it at least 1.
String formatBytes(num value) {
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  var v = value.toDouble();
  var unit = 0;
  while (v >= 1024 && unit < units.length - 1) {
    v /= 1024;
    unit++;
  }
  return unit == 0
      ? '${v.toInt()} B'
      : '${v.toStringAsFixed(1)} ${units[unit]}';
}

/// [value] with K/M/B for thousands, millions, billions.
String formatCount(num value) {
  final v = value.toDouble();
  String short(double d, String unit) =>
      '${d >= 100 ? d.toStringAsFixed(0) : d.toStringAsFixed(1)}$unit';
  if (v >= 1e9) return short(v / 1e9, 'B');
  if (v >= 1e6) return short(v / 1e6, 'M');
  if (v >= 1e4) return short(v / 1e3, 'K');
  return '${value.toInt()}';
}

/// A local date and time, from milliseconds since the epoch.
String formatTime(num? ms) {
  if (ms == null) return '—';
  final t = DateTime.fromMillisecondsSinceEpoch(ms.toInt()).toLocal();
  String two(int v) => v.toString().padLeft(2, '0');
  return '${t.year}-${two(t.month)}-${two(t.day)} ${two(t.hour)}:${two(t.minute)}';
}

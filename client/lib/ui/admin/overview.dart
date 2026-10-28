import 'dart:math';

import 'package:fl_chart/fl_chart.dart';
import 'package:flutter/material.dart';

import '../../controller.dart';
import '../../l10n.dart';
import '../../theme.dart';
import '../common.dart';
import '../sectioned.dart';
import 'admin.dart';

/// Instance figures over the last 7, 30 or 90 days, as of the last refresh.
class Overview extends StatefulWidget {
  final HubController hub;
  const Overview({super.key, required this.hub});
  @override
  State<Overview> createState() => _OverviewState();
}

class _OverviewState extends State<Overview> {
  final loader = GlobalKey<LoaderState<Map<String, dynamic>>>();
  int days = 7;

  Future<Map<String, dynamic>> load() async => Map<String, dynamic>.from(
    await widget.hub.admin(
      '/admin/stats?days=$days&tz_offset=${DateTime.now().timeZoneOffset.inMinutes}',
    ),
  );

  @override
  Widget build(BuildContext context) => Column(
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: [
      Row(
        children: [
          ChoicePill<int>(
            tooltip: tr('统计区间', 'Period'),
            value: days,
            options: [
              for (final d in [7, 30, 90]) (d, tr('近 $d 天', 'Last $d days')),
            ],
            onChanged: (v) {
              setState(() => days = v);
              loader.currentState?.reload();
            },
          ),
          const Spacer(),
          IconButton(
            tooltip: tr('刷新', 'Refresh'),
            icon: const Icon(Icons.refresh, size: 18),
            onPressed: () => loader.currentState?.reload(),
          ),
        ],
      ),
      const SizedBox(height: Space.edge),
      Loader<Map<String, dynamic>>(
        key: loader,
        load: load,
        builder: (context, stats, _) => _Figures(stats),
      ),
    ],
  );
}

num _n(Object? value) => value is num ? value : 0;

class _Figures extends StatelessWidget {
  final Map<String, dynamic> stats;
  const _Figures(this.stats);

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    Map get(String key) => stats[key] as Map? ?? {};
    final daily = (stats['daily'] as List? ?? []).cast<Map>();
    final users = get('users'), modelUsers = get('model_users');
    final conversations = get('conversations'), runs = get('runs');
    final nodes = get('nodes');
    final capacity = stats['node_capacity'];
    num series(Map day, String key) => _n(day[key]);
    final total = _n(stats['total_tokens']);
    final estimated = total == 0
        ? 0
        : (_n(stats['estimated_tokens']) * 100 / total).round();
    final retention = get('retention');
    String kept(Object? days) => _n(days) == 0
        ? tr('永久', 'forever')
        : tr('${_n(days)} 天', '${_n(days)} days');
    final last = daily.isEmpty ? '' : '${daily.last['day']}';
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Wrap(
          spacing: 12,
          runSpacing: 12,
          children: [
            _Stat(
              tr('用户', 'Users'),
              formatCount(_n(users['total'])),
              tr(
                '已禁用 ${_n(users['disabled'])} · 待删除 ${_n(users['deletion_pending'])}',
                '${_n(users['disabled'])} disabled · ${_n(users['deletion_pending'])} pending deletion',
              ),
            ),
            _Stat(
              tr('使用模型的用户', 'Users using models'),
              '${_n(modelUsers['week'])} / ${_n(modelUsers['month'])}',
              tr('近 7 天 / 近 30 天', 'Last 7 / 30 days'),
            ),
            _Stat(
              tr('对话', 'Conversations'),
              formatCount(
                _n(conversations['chat']) + _n(conversations['work']),
              ),
              'Chat ${_n(conversations['chat'])} · Work ${_n(conversations['work'])}',
            ),
            _Stat(
              tr('运行中的任务', 'Runs in progress'),
              '${_n(runs['running'])}',
              tr(
                '等待审批 ${_n(runs['requires_action'])}',
                '${_n(runs['requires_action'])} awaiting approval',
              ),
            ),
            _Stat(
              tr('运行中的沙箱', 'Running sandboxes'),
              '${_n(stats['sandboxes_running'])}',
              tr(
                '节点容量 ${capacity ?? '不限'}',
                'Node capacity ${capacity ?? 'unlimited'}',
              ),
            ),
            _Stat(
              tr('执行节点', 'Execution nodes'),
              '${nodes.values.fold<num>(0, (a, v) => a + _n(v))}',
              [for (final e in nodes.entries) '${e.key} ${e.value}']
                  .join(' · '),
            ),
            _Stat(
              tr('检查点大小', 'Checkpoint size'),
              formatBytes(_n(stats['checkpoint_bytes'])),
              null,
            ),
            _Stat(
              tr('附件大小', 'Attachment size'),
              formatBytes(_n(stats['attachment_bytes'])),
              null,
            ),
          ],
        ),
        const SizedBox(height: 20),
        _Grid([
          _Chart(
            tr('每日 token', 'Tokens per day'),
            [(c.accent, tr('输入', 'Input')), (c.success, tr('输出', 'Output'))],
            _Lines(daily, [
              (c.accent, (d) => series(d, 'input_tokens')),
              (c.success, (d) => series(d, 'output_tokens')),
            ]),
          ),
          _Chart(
            tr('每日任务', 'Runs per day'),
            [(c.accent, 'Chat'), (c.success, 'Work')],
            _Lines(daily, [
              (c.accent, (d) => series(d, 'chat_runs')),
              (c.success, (d) => series(d, 'work_runs')),
            ]),
          ),
          _Chart(
            tr('每日使用模型的用户', 'Users using models per day'),
            const [],
            _Lines(daily, [(c.accent, (d) => series(d, 'model_users'))]),
          ),
          _Chart(
            tr('每日新注册用户', 'New users per day'),
            const [],
            _Bars(
              [
                for (final d in daily)
                  (_day('${d['day']}'), series(d, 'new_users')),
              ],
              c.accent,
              vertical: true,
            ),
          ),
          _Chart(
            tr('各模型 token', 'Tokens by model'),
            const [],
            _Bars([
              for (final m in (stats['models'] as List? ?? []).cast<Map>())
                (
                  '${m['model']}',
                  _n(m['input_tokens']) + _n(m['output_tokens']),
                ),
            ], c.accent),
          ),
          _Chart(
            tr('各 Harness 任务数', 'Runs by harness'),
            const [],
            _Bars([
              for (final h in (stats['harnesses'] as List? ?? []).cast<Map>())
                (pathLabel('${h['harness']}'), _n(h['runs'])),
            ], c.success),
          ),
          _Chart(
            tr('任务结果', 'Run outcomes'),
            const [],
            _Pie(get('run_outcomes'), {
              'completed': (c.success, tr('完成', 'Completed')),
              'failed': (c.danger, tr('失败', 'Failed')),
              'cancelled': (c.subtle, tr('取消', 'Cancelled')),
            }),
          ),
          _Chart(
            tr('模型请求结果', 'Model requests'),
            const [],
            _Pie(get('request_statuses'), {
              'completed': (c.success, tr('完成', 'Completed')),
              'interrupted': (c.warning, tr('中断', 'Interrupted')),
              'upstream_error': (c.danger, tr('上游错误', 'Upstream error')),
            }),
          ),
        ]),
        const SizedBox(height: 20),
        SettingsCard(
          title: tr('token 用量最多的用户', 'Top users by tokens'),
          children: [
            for (final u in (stats['top_users'] as List? ?? []).cast<Map>())
              SettingRow(
                title: '${u['email']}',
                trailing: Text(
                  '${formatCount(_n(u['input_tokens']))} / ${formatCount(_n(u['output_tokens']))}',
                  style: TextStyle(fontSize: FontSizes.label, color: c.muted),
                ),
              ),
            if ((stats['top_users'] as List? ?? []).isEmpty)
              SettingRow(title: tr('这段时间没有用量', 'No usage in this period')),
          ],
        ),
        Wrap(
          spacing: Space.edge,
          children: [
            for (final note in [
              '${stats['first_day']} – $last',
              tr('估算 $estimated%', '$estimated% estimated'),
              tr(
                '用量保留 ${kept(retention['usage_days'])}',
                'Usage kept ${kept(retention['usage_days'])}',
              ),
              tr(
                '审计保留 ${kept(retention['audit_days'])}',
                'Audit kept ${kept(retention['audit_days'])}',
              ),
            ])
              Text(
                note,
                style: TextStyle(fontSize: FontSizes.caption, color: c.subtle),
              ),
          ],
        ),
      ],
    );
  }
}

/// `MM-DD` of a `YYYY-MM-DD` day.
String _day(String day) => day.length == 10 ? day.substring(5) : day;

class _Stat extends StatelessWidget {
  final String title, value;
  final String? detail;
  const _Stat(this.title, this.value, this.detail);
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Container(
      width: 200,
      padding: const EdgeInsets.all(Space.edge),
      decoration: BoxDecoration(
        borderRadius: BorderRadius.circular(Radii.card),
        border: Border.all(color: c.border),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            title,
            style: TextStyle(fontSize: FontSizes.label, color: c.subtle),
          ),
          const SizedBox(height: 6),
          Text(
            value,
            style: TextStyle(
              fontSize: 22,
              fontWeight: FontWeight.w500,
              color: c.text,
            ),
          ),
          if (detail != null && detail!.isNotEmpty) ...[
            const SizedBox(height: 4),
            Text(
              detail!,
              style: TextStyle(fontSize: FontSizes.caption, color: c.muted),
            ),
          ],
        ],
      ),
    );
  }
}

/// Charts two to a row where there is room.
class _Grid extends StatelessWidget {
  final List<Widget> children;
  const _Grid(this.children);
  @override
  Widget build(BuildContext context) => LayoutBuilder(
    builder: (context, constraints) {
      final columns = constraints.maxWidth >= 720 ? 2 : 1;
      final width = (constraints.maxWidth - 12 * (columns - 1)) / columns;
      return Wrap(
        spacing: 12,
        runSpacing: 12,
        children: [
          for (final child in children) SizedBox(width: width, child: child),
        ],
      );
    },
  );
}

class _Chart extends StatelessWidget {
  final String title;
  final List<(Color, String)> legend;
  final Widget chart;
  const _Chart(this.title, this.legend, this.chart);
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Container(
      padding: const EdgeInsets.all(Space.edge),
      decoration: BoxDecoration(
        borderRadius: BorderRadius.circular(Radii.card),
        border: Border.all(color: c.border),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Expanded(
                child: Text(
                  title,
                  style: TextStyle(fontSize: FontSizes.label, color: c.text),
                ),
              ),
              for (final (color, label) in legend) ...[
                const SizedBox(width: 10),
                Container(width: 8, height: 8, color: color),
                const SizedBox(width: 4),
                Text(
                  label,
                  style: TextStyle(fontSize: FontSizes.caption, color: c.muted),
                ),
              ],
            ],
          ),
          const SizedBox(height: Space.edge),
          SizedBox(height: 180, child: chart),
        ],
      ),
    );
  }
}

FlTitlesData _titles(
  BuildContext context, {
  required double top,
  required String Function(double) bottom,
  double? interval,
  double reserved = 22,
}) {
  final style = TextStyle(fontSize: 10, color: HubColors.of(context).subtle);
  return FlTitlesData(
    topTitles: const AxisTitles(),
    rightTitles: const AxisTitles(),
    leftTitles: AxisTitles(
      sideTitles: SideTitles(
        showTitles: true,
        reservedSize: 40,
        interval: _step(top),
        maxIncluded: false,
        getTitlesWidget: (value, meta) => SideTitleWidget(
          meta: meta,
          child: Text(formatCount(value), style: style),
        ),
      ),
    ),
    bottomTitles: AxisTitles(
      sideTitles: SideTitles(
        showTitles: true,
        reservedSize: reserved,
        interval: interval,
        maxIncluded: false,
        getTitlesWidget: (value, meta) => SideTitleWidget(
          meta: meta,
          child: Text(
            bottom(value),
            style: style,
            overflow: TextOverflow.ellipsis,
          ),
        ),
      ),
    ),
  );
}

/// The top of an axis that holds [values], and is never empty.
double _top(Iterable<num> values) =>
    max(1, values.fold<num>(0, (a, v) => max(a, v)).toDouble() * 1.1);

/// A round step that marks an axis up to [top] about four times, and whole
/// numbers only.
double _step(double top) {
  final raw = top / 4;
  if (raw <= 1) return 1;
  final magnitude = pow(10, (log(raw) / ln10).floor()).toDouble();
  for (final m in [1, 2, 5]) {
    if (raw <= m * magnitude) return m * magnitude;
  }
  return 10 * magnitude;
}

/// Daily series, one line each.
class _Lines extends StatelessWidget {
  final List<Map> days;
  final List<(Color, num Function(Map))> lines;
  const _Lines(this.days, this.lines);
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final top = _top([for (final (_, value) in lines) ...days.map(value)]);
    return LineChart(
      LineChartData(
        minY: 0,
        maxY: top,
        gridData: FlGridData(
          drawVerticalLine: false,
          horizontalInterval: _step(top),
          getDrawingHorizontalLine: (_) =>
              FlLine(color: c.border, strokeWidth: 1),
        ),
        borderData: FlBorderData(show: false),
        titlesData: _titles(
          context,
          top: top,
          interval: max(1, (days.length / 6).ceilToDouble()),
          bottom: (x) {
            final i = x.toInt();
            return i >= 0 && i < days.length ? _day('${days[i]['day']}') : '';
          },
        ),
        lineBarsData: [
          for (final (color, value) in lines)
            LineChartBarData(
              spots: [
                for (final (i, d) in days.indexed)
                  FlSpot(i.toDouble(), value(d).toDouble()),
              ],
              color: color,
              barWidth: 2,
              dotData: const FlDotData(show: false),
            ),
        ],
      ),
    );
  }
}

/// One bar per label; horizontal unless [vertical], which suits many short
/// labels such as days.
class _Bars extends StatelessWidget {
  final List<(String, num)> bars;
  final Color color;
  final bool vertical;
  const _Bars(this.bars, this.color, {this.vertical = false});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    if (bars.isEmpty || bars.every((b) => b.$2 == 0)) {
      return Center(child: EmptyHint(tr('暂无数据', 'No data')));
    }
    final top = _top(bars.map((b) => b.$2));
    return BarChart(
      BarChartData(
        rotationQuarterTurns: vertical ? 0 : 1,
        minY: 0,
        maxY: top,
        gridData: FlGridData(
          drawVerticalLine: false,
          horizontalInterval: _step(top),
          getDrawingHorizontalLine: (_) =>
              FlLine(color: c.border, strokeWidth: 1),
        ),
        borderData: FlBorderData(show: false),
        titlesData: _titles(
          context,
          top: top,
          reserved: vertical ? 22 : 96,
          bottom: (x) {
            final i = x.toInt();
            if (i < 0 || i >= bars.length) return '';
            // Days in turn, every few when there are many.
            final every = vertical ? max(1, (bars.length / 6).ceil()) : 1;
            return i % every == 0 ? bars[i].$1 : '';
          },
        ),
        barGroups: [
          for (final (i, (_, value)) in bars.indexed)
            BarChartGroupData(
              x: i,
              barRods: [
                BarChartRodData(
                  toY: value.toDouble(),
                  color: color,
                  width: vertical ? max(2, 240 / bars.length) : 14,
                  borderRadius: BorderRadius.circular(2),
                ),
              ],
            ),
        ],
      ),
    );
  }
}

/// Shares of [counts] by key, in the order and colours of [keys].
class _Pie extends StatelessWidget {
  final Map counts;
  final Map<String, (Color, String)> keys;
  const _Pie(this.counts, this.keys);
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final total = keys.keys.fold<num>(0, (a, k) => a + _n(counts[k]));
    if (total == 0) return Center(child: EmptyHint(tr('暂无数据', 'No data')));
    return Row(
      children: [
        Expanded(
          child: PieChart(
            PieChartData(
              centerSpaceRadius: 36,
              sectionsSpace: 1,
              sections: [
                for (final MapEntry(:key, value: (color, _)) in keys.entries)
                  if (_n(counts[key]) > 0)
                    PieChartSectionData(
                      value: _n(counts[key]).toDouble(),
                      color: color,
                      radius: 40,
                      showTitle: false,
                    ),
              ],
            ),
          ),
        ),
        Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            for (final MapEntry(:key, value: (color, label)) in keys.entries)
              Padding(
                padding: const EdgeInsets.symmetric(vertical: 3),
                child: Row(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Container(width: 8, height: 8, color: color),
                    const SizedBox(width: 6),
                    Text(
                      '$label ${_n(counts[key])} · ${(_n(counts[key]) * 100 / total).round()}%',
                      style: TextStyle(
                        fontSize: FontSizes.caption,
                        color: c.muted,
                      ),
                    ),
                  ],
                ),
              ),
          ],
        ),
      ],
    );
  }
}

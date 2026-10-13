import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:url_launcher/url_launcher.dart';

import '../controller.dart';
import '../l10n.dart';
import '../theme.dart';
import 'code.dart';
import 'dialogs.dart';
import 'plugin_app.dart';

/// Gives timeline widgets the controller, for what plugin results do: call tools, open apps.
class PluginHost extends InheritedWidget {
  final HubController hub;
  const PluginHost({super.key, required this.hub, required super.child});
  static HubController? of(BuildContext context) =>
      context.dependOnInheritedWidgetOfExactType<PluginHost>()?.hub;
  @override
  bool updateShouldNotify(PluginHost old) => old.hub != hub;
}

/// The extension key a plugin's tool result carries declarative UI under.
const uiKey = 'app.ryzhub/ui';

/// The model's name for a plugin's tool, as the server forms it.
String pluginToolName(String plugin, String tool) {
  String clean(String text) => text.replaceAll(RegExp(r'[^A-Za-z0-9_-]'), '_');
  return tool.contains('__') ? tool : '${clean(plugin)}__${clean(tool)}';
}

/// What a plugin tool's result shows beyond its text: declarative components, and its MCP
/// App page. Nothing for other tools.
class PluginResult extends StatelessWidget {
  final Map<String, dynamic> item;
  const PluginResult({super.key, required this.item});

  @override
  Widget build(BuildContext context) {
    final tool = item['detail']?['kind']?['Tool'];
    if (tool is! Map || '${tool['plugin'] ?? ''}'.isEmpty) {
      return const SizedBox.shrink();
    }
    final hub = PluginHost.of(context);
    Map? result;
    try {
      final decoded = jsonDecode('${tool['result_json'] ?? ''}');
      if (decoded is Map) result = decoded;
    } on FormatException {
      result = null;
    }
    final component = (result?['_meta'] as Map?)?[uiKey];
    final app = '${tool['app'] ?? ''}';
    if (hub == null || (component is! Map && app.isEmpty)) {
      return const SizedBox.shrink();
    }
    return Padding(
      padding: const EdgeInsets.fromLTRB(0, Space.s4, 0, Space.s8),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          if (component is Map)
            PluginComponent(
              component: Map<String, dynamic>.from(component),
              call: (name, arguments) => hub.pluginToolCall(
                '${item['id']}',
                pluginToolName('${tool['plugin']}', name),
                arguments,
              ),
            ),
          if (app.isNotEmpty)
            PluginAppPanel(
              hub: hub,
              item: '${item['id']}',
              plugin: '${tool['plugin']}',
            ),
        ],
      ),
    );
  }
}

typedef ToolCaller = Future<dynamic> Function(
  String tool,
  Map<String, dynamic> arguments,
);

/// A declarative component from a plugin's catalog of kinds: card, text, list, table, button,
/// form, image, chart, code, diff. Anything else is left out. Buttons and forms call the
/// plugin's tools through the same gateway and rules as the model.
class PluginComponent extends StatefulWidget {
  final Map<String, dynamic> component;
  final ToolCaller call;
  const PluginComponent({
    super.key,
    required this.component,
    required this.call,
  });
  @override
  State<PluginComponent> createState() => _PluginComponentState();
}

class _PluginComponentState extends State<PluginComponent> {
  final fields = <String, TextEditingController>{};
  String? outcome;
  bool busy = false;

  @override
  void dispose() {
    for (final controller in fields.values) {
      controller.dispose();
    }
    super.dispose();
  }

  Future<void> run(Map action, [Map<String, dynamic> values = const {}]) async {
    setState(() {
      busy = true;
      outcome = null;
    });
    try {
      final result = await widget.call('${action['tool']}', {
        ...Map<String, dynamic>.from(action['arguments'] as Map? ?? {}),
        ...values,
      });
      final content = (result is Map ? result['content'] as List? : null) ?? [];
      outcome = content
          .whereType<Map>()
          .map((c) => '${c['text'] ?? ''}')
          .join('\n')
          .trim();
      if (outcome!.isEmpty) outcome = tr('已完成', 'Done');
    } catch (e) {
      outcome = errorText(e);
    }
    if (mounted) setState(() => busy = false);
  }

  Widget child(Object? node) => node is Map
      ? PluginComponent(
          component: Map<String, dynamic>.from(node),
          call: widget.call,
        )
      : Text('${node ?? ''}');

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final node = widget.component;
    final mono = TextStyle(
      fontFamily: 'monospace',
      fontFamilyFallback: const ['RyzHub Sans'],
      fontSize: 12.5,
      height: 1.45,
      color: c.text,
    );
    final body = switch (node['type']) {
      'card' => Container(
        padding: const EdgeInsets.all(Space.s12),
        decoration: BoxDecoration(
          border: Border.all(color: c.border),
          borderRadius: BorderRadius.circular(10),
        ),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            if (node['title'] != null)
              Text(
                '${node['title']}',
                style: TextStyle(fontWeight: FontWeight.w600, color: c.text),
              ),
            for (final item in (node['children'] as List? ?? []))
              Padding(
                padding: const EdgeInsets.only(top: Space.s6),
                child: child(item),
              ),
          ],
        ),
      ),
      'text' => SelectableText(
        '${node['text'] ?? ''}',
        style: TextStyle(
          color: node['style'] == 'muted' ? c.muted : c.text,
          fontWeight: node['style'] == 'title' ? FontWeight.w600 : null,
        ),
      ),
      'list' => Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          for (final item in (node['items'] as List? ?? []))
            Row(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                const Text('• '),
                Expanded(child: child(item)),
              ],
            ),
        ],
      ),
      'table' => SingleChildScrollView(
        scrollDirection: Axis.horizontal,
        child: DataTable(
          headingRowHeight: 32,
          dataRowMinHeight: 28,
          dataRowMaxHeight: 48,
          columns: [
            for (final column in (node['columns'] as List? ?? []))
              DataColumn(label: Text('$column')),
          ],
          rows: [
            for (final row in (node['rows'] as List? ?? []))
              if (row is List &&
                  row.length == (node['columns'] as List? ?? []).length)
                DataRow(
                  cells: [for (final cell in row) DataCell(Text('$cell'))],
                ),
          ],
        ),
      ),
      'button' => Align(
        alignment: Alignment.centerLeft,
        child: OutlinedButton(
          onPressed: busy || node['action'] is! Map
              ? null
              : () => run(node['action'] as Map),
          child: Text('${node['label'] ?? tr('运行', 'Run')}'),
        ),
      ),
      'form' => Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          for (final field in (node['fields'] as List? ?? []).whereType<Map>())
            TextField(
              controller: fields.putIfAbsent(
                '${field['name']}',
                TextEditingController.new,
              ),
              decoration: InputDecoration(
                labelText: '${field['label'] ?? field['name']}',
              ),
              keyboardType: field['type'] == 'number'
                  ? TextInputType.number
                  : null,
            ),
          const SizedBox(height: 8),
          Align(
            alignment: Alignment.centerRight,
            child: FilledButton(
              onPressed: busy || node['submit'] is! Map
                  ? null
                  : () => run(node['submit'] as Map, {
                      for (final MapEntry(:key, :value) in fields.entries)
                        if (value.text.isNotEmpty) key: value.text,
                    }),
              child: Text(
                '${(node['submit'] as Map?)?['label'] ?? tr('提交', 'Submit')}',
              ),
            ),
          ),
        ],
      ),
      'image' => _image(context, node),
      'chart' => _BarChart(node: node),
      'code' => CodeText(
        '${node['code'] ?? ''}',
        language: node['language'] as String?,
        style: mono,
      ),
      'diff' => DiffText('${node['diff'] ?? ''}', style: mono),
      _ => const SizedBox.shrink(),
    };
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        body,
        if (outcome != null)
          Padding(
            padding: const EdgeInsets.only(top: Space.s6),
            child: SelectableText(
              outcome!,
              style: TextStyle(fontSize: 12, color: c.muted),
            ),
          ),
      ],
    );
  }

  /// Images are shown only when they come with the result; addresses stay links, so that a
  /// plugin cannot see when its result is read.
  Widget _image(BuildContext context, Map node) {
    final src = '${node['src'] ?? ''}';
    final alt = '${node['alt'] ?? tr('图片', 'Image')}';
    final data = Uri.tryParse(src)?.data;
    if (data != null && data.mimeType.startsWith('image/')) {
      return Image.memory(data.contentAsBytes(), semanticLabel: alt);
    }
    return Align(
      alignment: Alignment.centerLeft,
      child: TextButton.icon(
        icon: const Icon(Icons.image_outlined, size: 16),
        label: Text(alt),
        onPressed: Uri.tryParse(src)?.scheme == 'https'
            ? () => launchUrl(Uri.parse(src), webOnlyWindowName: '_blank')
            : null,
      ),
    );
  }
}

/// A bar chart: `labels`, and `series` of `{name, values}`.
class _BarChart extends StatelessWidget {
  final Map node;
  const _BarChart({required this.node});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final labels = (node['labels'] as List? ?? []).map((l) => '$l').toList();
    final series = (node['series'] as List? ?? []).whereType<Map>().toList();
    final values = [
      for (final s in series) ...(s['values'] as List? ?? []).whereType<num>(),
    ];
    final top = values.isEmpty
        ? 1.0
        : values.reduce((a, b) => a > b ? a : b).toDouble();
    final palette = [c.accent, c.success, c.warning, c.danger];
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (node['title'] != null)
          Text('${node['title']}', style: TextStyle(color: c.text)),
        for (final (i, label) in labels.indexed)
          Padding(
            padding: const EdgeInsets.only(top: Space.s4),
            child: Row(
              children: [
                SizedBox(
                  width: 90,
                  child: Text(
                    label,
                    overflow: TextOverflow.ellipsis,
                    style: const TextStyle(fontSize: 12),
                  ),
                ),
                Expanded(
                  child: Column(
                    children: [
                      for (final (n, s) in series.indexed)
                        if (i < (s['values'] as List? ?? []).length &&
                            (s['values'] as List)[i] is num)
                          Row(
                            children: [
                              Flexible(
                                child: FractionallySizedBox(
                                  alignment: Alignment.centerLeft,
                                  widthFactor: top <= 0
                                      ? 0
                                      : (((s['values'] as List)[i] as num) /
                                                top)
                                            .clamp(0, 1)
                                            .toDouble(),
                                  child: Container(
                                    height: 10,
                                    color: palette[n % palette.length],
                                  ),
                                ),
                              ),
                              const SizedBox(width: 6),
                              Text(
                                '${(s['values'] as List)[i]}',
                                style: const TextStyle(fontSize: 11),
                              ),
                            ],
                          ),
                    ],
                  ),
                ),
              ],
            ),
          ),
      ],
    );
  }
}

/// An MCP App page for a tool item, opened on request. The page runs on the plugin content
/// origin, sandboxed; the bridge gives it only this call's input and result, tool calls checked
/// by the server, and messages the member confirms.
class PluginAppPanel extends StatefulWidget {
  final HubController hub;
  final String item, plugin;
  const PluginAppPanel({
    super.key,
    required this.hub,
    required this.item,
    required this.plugin,
  });
  @override
  State<PluginAppPanel> createState() => _PluginAppPanelState();
}

class _PluginAppPanelState extends State<PluginAppPanel> {
  Map<String, dynamic>? opened;
  String error = '';
  bool busy = false;
  double height = 320;

  Future<void> open() async {
    setState(() {
      busy = true;
      error = '';
    });
    try {
      final data = await widget.hub.request(
        '/orgs/${widget.hub.org}/conversations/${widget.hub.selected}/apps',
        body: {'item_id': widget.item},
        timeout: const Duration(seconds: 60),
      );
      opened = Map<String, dynamic>.from(data);
    } catch (e) {
      error = errorText(e);
    }
    if (mounted) setState(() => busy = false);
  }

  AppBridge bridge(BuildContext context, Map<String, dynamic> opened) =>
      AppBridge(
        toolInput: opened['arguments'],
        toolResult: opened['result'],
        dark: Theme.of(context).brightness == Brightness.dark,
        callTool: (name, arguments) => widget.hub.pluginToolCall(
          widget.item,
          pluginToolName(widget.plugin, name),
          arguments,
        ),
        sendMessage: (text) async {
          if (!context.mounted) return false;
          final ok = await confirmDialog(
            context,
            title: tr('插件请求发送消息', 'The plugin asks to send a message'),
            message: text,
            confirm: tr('发送', 'Send'),
          );
          if (ok) await widget.hub.send(text);
          return ok;
        },
        openLink: (url) async {
          final uri = Uri.tryParse(url);
          if (uri == null || uri.scheme != 'https') return false;
          return launchUrl(uri, webOnlyWindowName: '_blank');
        },
        resize: (value) {
          if (mounted) setState(() => height = value.clamp(80, 900).toDouble());
        },
      );

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final shown = opened;
    if (shown == null) {
      return Align(
        alignment: Alignment.centerLeft,
        child: Wrap(
          crossAxisAlignment: WrapCrossAlignment.center,
          spacing: 8,
          children: [
            OutlinedButton.icon(
              icon: const Icon(Icons.web_asset, size: 16),
              label: Text(
                tr('打开 ${widget.plugin} 界面', 'Open ${widget.plugin} view'),
              ),
              onPressed: busy ? null : open,
            ),
            if (error.isNotEmpty)
              Text(error, style: TextStyle(color: c.danger, fontSize: 12)),
          ],
        ),
      );
    }
    return ClipRRect(
      borderRadius: BorderRadius.circular(10),
      child: DecoratedBox(
        decoration: BoxDecoration(border: Border.all(color: c.border)),
        child: AppFrame(
          url: '${shown['url']}',
          origin: '${shown['origin']}',
          height: height,
          bridge: bridge(context, shown),
        ),
      ),
    );
  }
}

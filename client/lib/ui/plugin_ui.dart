import 'package:flutter/material.dart';
import 'package:url_launcher/url_launcher.dart';

import '../controller.dart';
import '../l10n.dart';
import '../theme.dart';
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

/// The model's name for a plugin's tool, as the server forms it.
String pluginToolName(String plugin, String tool) {
  String clean(String text) => text.replaceAll(RegExp(r'[^A-Za-z0-9_-]'), '_');
  return tool.contains('__') ? tool : '${clean(plugin)}__${clean(tool)}';
}

/// A plugin tool's MCP App page, opened from its result. Nothing for other
/// tools.
class PluginResult extends StatelessWidget {
  final Map<String, dynamic> item;
  const PluginResult({super.key, required this.item});

  @override
  Widget build(BuildContext context) {
    final tool = item['detail']?['kind']?['Tool'];
    final hub = PluginHost.of(context);
    if (hub == null ||
        tool is! Map ||
        '${tool['plugin'] ?? ''}'.isEmpty ||
        '${tool['app'] ?? ''}'.isEmpty) {
      return const SizedBox.shrink();
    }
    return Padding(
      padding: const EdgeInsets.fromLTRB(0, 4, 0, Space.gap),
      child: PluginAppPanel(
        hub: hub,
        item: '${item['id']}',
        plugin: '${tool['plugin']}',
      ),
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
      final data = await widget.hub.pluginApp(widget.item);
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
          spacing: Space.gap,
          children: [
            OutlinedButton.icon(
              icon: const Icon(Icons.web_asset, size: 16),
              label: Text(
                tr('打开 ${widget.plugin} 界面', 'Open ${widget.plugin} view'),
              ),
              onPressed: busy ? null : open,
            ),
            if (error.isNotEmpty)
              Text(
                error,
                style: TextStyle(color: c.danger, fontSize: FontSizes.caption),
              ),
          ],
        ),
      );
    }
    return ClipRRect(
      borderRadius: BorderRadius.circular(Radii.control),
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

import '../l10n.dart';

/// The host side of an MCP App: JSON-RPC over `postMessage`. The page gets this call's input
/// and result, may call tools (the server checks them), open HTTPS links, ask to send a message
/// (the member confirms) and size its frame. Nothing else of the conversation reaches it.
class AppBridge {
  final Object? toolInput, toolResult;
  final bool dark;
  final Future<dynamic> Function(String tool, Map<String, dynamic> arguments)
  callTool;
  final Future<bool> Function(String text) sendMessage;
  final Future<bool> Function(String url) openLink;
  final void Function(double height) resize;
  const AppBridge({
    required this.toolInput,
    required this.toolResult,
    required this.dark,
    required this.callTool,
    required this.sendMessage,
    required this.openLink,
    required this.resize,
  });

  /// The answers to one message from the page, and what follows them.
  Future<List<Map<String, dynamic>>> handle(Object? message) async {
    if (message is! Map) return const [];
    final id = message['id'];
    final params = message['params'] is Map
        ? Map<String, dynamic>.from(message['params'] as Map)
        : <String, dynamic>{};
    Map<String, dynamic> reply(Object? result) => {
      'jsonrpc': '2.0',
      'id': id,
      'result': result,
    };
    Map<String, dynamic> fail(String text) => {
      'jsonrpc': '2.0',
      'id': id,
      'error': {'code': -32000, 'message': text},
    };
    switch (message['method']) {
      case 'ui/initialize':
        return [
          reply({
            'protocolVersion': '2026-01-26',
            'hostInfo': {'name': 'RyzHub', 'version': '1'},
            'hostCapabilities': {
              'serverTools': <String, dynamic>{},
              'openLinks': <String, dynamic>{},
              'message': <String, dynamic>{},
            },
            'hostContext': {
              'theme': dark ? 'dark' : 'light',
              'displayMode': 'inline',
            },
          }),
        ];
      case 'ui/notifications/initialized':
        return [
          {
            'jsonrpc': '2.0',
            'method': 'ui/notifications/tool-input',
            'params': {'arguments': toolInput},
          },
          {
            'jsonrpc': '2.0',
            'method': 'ui/notifications/tool-result',
            'params': toolResult,
          },
        ];
      case 'tools/call':
        try {
          final arguments = params['arguments'];
          return [
            reply(
              await callTool(
                '${params['name']}',
                arguments is Map ? Map<String, dynamic>.from(arguments) : {},
              ),
            ),
          ];
        } catch (e) {
          return [fail(errorText(e))];
        }
      case 'ui/message':
        final content = params['content'];
        final text = content is List
            ? content
                  .whereType<Map>()
                  .map((c) => '${c['text'] ?? ''}')
                  .join('\n')
            : '${params['text'] ?? ''}';
        if (text.trim().isEmpty) return [fail('empty message')];
        return [(await sendMessage(text)) ? reply({}) : fail('declined')];
      case 'ui/open-link':
        return [
          (await openLink('${params['url']}')) ? reply({}) : fail('declined'),
        ];
      case 'ui/notifications/size-changed':
        final height = params['height'];
        if (height is num) resize(height.toDouble());
        return const [];
      default:
        return id == null ? const [] : [fail('unsupported')];
    }
  }
}

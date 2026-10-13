import 'dart:convert';
import 'dart:js_interop';
import 'dart:ui_web' as ui_web;

import 'package:flutter/widgets.dart';
import 'package:web/web.dart' as web;

import 'plugin_app_bridge.dart';

int _views = 0;

/// The page in an iframe sandboxed to scripts and forms: an opaque origin, no access to the
/// client's storage or cookies. Messages count only from that frame.
class AppFrame extends StatefulWidget {
  final String url, origin;
  final double height;
  final AppBridge bridge;
  const AppFrame({
    super.key,
    required this.url,
    required this.origin,
    required this.height,
    required this.bridge,
  });
  @override
  State<AppFrame> createState() => _AppFrameState();
}

class _AppFrameState extends State<AppFrame> {
  late final String view = 'ryzhub-plugin-app-${_views++}';
  web.HTMLIFrameElement? frame;
  JSFunction? listener;

  @override
  void initState() {
    super.initState();
    ui_web.platformViewRegistry.registerViewFactory(view, (int _) {
      final element = web.HTMLIFrameElement()
        ..src = widget.url
        ..setAttribute('sandbox', 'allow-scripts allow-forms')
        ..setAttribute('referrerpolicy', 'no-referrer');
      element.style
        ..border = 'none'
        ..width = '100%'
        ..height = '100%';
      frame = element;
      return element;
    });
    listener = ((web.MessageEvent event) {
      final window = frame?.contentWindow;
      if (window == null || !event.source.strictEquals(window).toDart) return;
      final data = event.data.dartify();
      final message = data is String ? _decode(data) : data;
      widget.bridge.handle(message).then((replies) {
        for (final reply in replies) {
          frame?.contentWindow?.postMessage(jsonEncode(reply).toJS, '*'.toJS);
        }
      });
    }).toJS;
    web.window.addEventListener('message', listener);
  }

  Object? _decode(String text) {
    try {
      return jsonDecode(text);
    } on FormatException {
      return null;
    }
  }

  @override
  void dispose() {
    web.window.removeEventListener('message', listener);
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => SizedBox(
    height: widget.height,
    child: HtmlElementView(viewType: view),
  );
}

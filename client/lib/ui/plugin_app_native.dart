import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:path_provider/path_provider.dart';
import 'package:webview_flutter/webview_flutter.dart';
import 'package:webview_windows/webview_windows.dart' as windows;

import '../l10n.dart';
import 'plugin_app_bridge.dart';

/// Makes the page's `window.parent.postMessage` reach the host, whatever the webview.
const _shim = '''
(function () {
  if (window.__ryzhub) return;
  window.__ryzhub = true;
  var post = window.chrome && window.chrome.webview
    ? function (m) { window.chrome.webview.postMessage(JSON.stringify(m)); }
    : function (m) { RyzhubHost.postMessage(JSON.stringify(m)); };
  window.parent = { postMessage: function (m) { post(m); } };
  if (window.chrome && window.chrome.webview) {
    window.chrome.webview.addEventListener('message', function (e) {
      window.dispatchEvent(new MessageEvent('message', { data: JSON.parse(e.data) }));
    });
  }
})();
''';

/// The page in a webview of its own: no shared cookies or files, navigation kept to the plugin
/// content origin.
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
  WebViewController? mobile;
  windows.WebviewController? desktop;
  String error = '';

  @override
  void initState() {
    super.initState();
    if (Platform.isWindows) {
      startWindows();
    } else if (Platform.isAndroid || Platform.isIOS) {
      startMobile();
    } else {
      error = tr('此平台暂不支持插件界面。', 'Plugin views are not supported here yet.');
    }
  }

  bool allowed(String url) => url.startsWith('${widget.origin}/');

  Future<void> deliver(
    Object? message,
    Future<void> Function(String) post,
  ) async {
    for (final reply in await widget.bridge.handle(message)) {
      await post(jsonEncode(reply));
    }
  }

  void startMobile() {
    // Plugin pages keep no cookies between them.
    WebViewCookieManager().clearCookies();
    final controller = WebViewController();
    controller
      ..setJavaScriptMode(JavaScriptMode.unrestricted)
      ..addJavaScriptChannel(
        'RyzhubHost',
        onMessageReceived: (message) => deliver(
          _decode(message.message),
          (reply) => controller.runJavaScript(
            'window.dispatchEvent(new MessageEvent("message",{data:JSON.parse(${jsonEncode(reply)})}))',
          ),
        ),
      )
      ..setNavigationDelegate(
        NavigationDelegate(
          onNavigationRequest: (request) => allowed(request.url)
              ? NavigationDecision.navigate
              : NavigationDecision.prevent,
          onPageStarted: (_) => controller.runJavaScript(_shim),
        ),
      )
      ..loadRequest(Uri.parse(widget.url));
    mobile = controller;
  }

  Future<void> startWindows() async {
    try {
      // A data folder of its own: nothing shared with other pages or the system browser.
      final folder = await getApplicationSupportDirectory();
      await windows.WebviewController.initializeEnvironment(
        userDataPath: '${folder.path}${Platform.pathSeparator}plugin-apps',
      );
    } catch (_) {
      // Already set up by an earlier page.
    }
    final controller = windows.WebviewController();
    try {
      await controller.initialize();
      await controller.addScriptToExecuteOnDocumentCreated(_shim);
      controller.webMessage.listen(
        (message) => deliver(
          message is String ? _decode(message) : message,
          (reply) => controller.postWebMessage(jsonEncode(reply)),
        ),
      );
      controller.url.listen((url) {
        if (!allowed(url)) controller.loadUrl(widget.url);
      });
      await controller.loadUrl(widget.url);
      if (mounted) setState(() => desktop = controller);
    } catch (e) {
      if (mounted) setState(() => error = '$e');
    }
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
    desktop?.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final view = mobile != null
        ? WebViewWidget(controller: mobile!)
        : desktop != null
        ? windows.Webview(desktop!)
        : Center(child: Text(error));
    return SizedBox(height: widget.height, child: view);
  }
}

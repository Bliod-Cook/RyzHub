/// An MCP App page in a frame of its own: an iframe on the web, a webview elsewhere.
library;

export 'plugin_app_bridge.dart';
export 'plugin_app_native.dart'
    if (dart.library.js_interop) 'plugin_app_web.dart';

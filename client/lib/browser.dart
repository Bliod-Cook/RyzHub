/// What the client gets from where it runs: the web build keeps a signed-in
/// device through the server's refresh cookie; native builds do not yet.
library;

export 'browser_native.dart' if (dart.library.js_interop) 'browser_web.dart';

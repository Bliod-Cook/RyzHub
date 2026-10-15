/// What the client gets from where it runs: how it keeps the device signed in
/// (the browser's refresh cookie, or the platform's secure storage), saves,
/// takes in and hashes files, and signs in with other accounts.
library;

export 'browser_native.dart' if (dart.library.js_interop) 'browser_web.dart';

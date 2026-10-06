import 'package:http/browser_client.dart';
import 'package:http/http.dart' as http;
import 'package:web/web.dart' as web;

/// The browser holds the device's refresh token in an HttpOnly cookie.
const keepsSession = true;

/// Requests carry the cookie to the server, which is another origin.
http.Client sessionClient() => BrowserClient()..withCredentials = true;

const _loopback = {'localhost', '127.0.0.1', '[::1]', '::1'};

/// [server] reached through the page's own loopback name, so the refresh
/// cookie stays same-site: a page on `localhost` calling `127.0.0.1` is
/// cross-site, and browsers withhold its cookies.
String sameSiteServer(String server) {
  final uri = Uri.tryParse(server);
  final page = web.window.location.hostname;
  if (uri == null ||
      uri.host == page ||
      !_loopback.contains(uri.host) ||
      !_loopback.contains(page)) {
    return server;
  }
  return uri.replace(host: page).toString().replaceAll(RegExp(r'/+$'), '');
}

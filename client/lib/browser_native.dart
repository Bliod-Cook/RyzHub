import 'package:http/http.dart' as http;

/// Native builds sign in again on each start until the refresh token has a
/// home in the platform's secure storage.
const keepsSession = false;

http.Client sessionClient() => http.Client();

String sameSiteServer(String server) => server;

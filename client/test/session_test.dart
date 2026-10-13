import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:ryzhub/browser_native.dart';

class _Memory implements SecretStore {
  final values = <String, String>{};

  @override
  Future<String?> read(String key) async => values[key];

  @override
  Future<void> write(String key, String value) async => values[key] = value;

  @override
  Future<void> delete(String key) async => values.remove(key);
}

void main() {
  test('the refresh cookie is kept per server and sent to /auth only', () async {
    final seen = <String, String?>{};
    final secrets = _Memory();
    var issued = 0;
    final client = RefreshCookieClient(
      MockClient((request) async {
        seen[request.url.path] = request.headers['cookie'];
        if (request.url.path == '/auth/login' ||
            request.url.path == '/auth/refresh') {
          issued++;
          return http.Response(
            '{}',
            200,
            headers: {
              'set-cookie':
                  'ryzhub_refresh=r$issued; Path=/auth; Max-Age=2592000; HttpOnly; SameSite=Strict',
            },
          );
        }
        if (request.url.path == '/auth/logout') {
          return http.Response(
            '',
            204,
            headers: {
              'set-cookie': 'ryzhub_refresh=; Path=/auth; Max-Age=0; HttpOnly; SameSite=Strict',
            },
          );
        }
        return http.Response('[]', 200);
      }),
      secrets,
    );
    await client.post(Uri.parse('https://hub.example/auth/login'));
    expect(secrets.values, {'refresh:https://hub.example': 'r1'});
    await client.get(Uri.parse('https://hub.example/orgs/o/conversations'));
    expect(seen['/orgs/o/conversations'], isNull, reason: 'not beyond /auth');
    // Another start: a new client reads it back from storage; rotation replaces it.
    final again = RefreshCookieClient(
      MockClient((request) async {
        seen[request.url.path] = request.headers['cookie'];
        return http.Response(
          '{}',
          200,
          headers: {'set-cookie': 'ryzhub_refresh=r9; Path=/auth; Max-Age=60'},
        );
      }),
      secrets,
    );
    await again.post(Uri.parse('https://hub.example/auth/refresh'));
    expect(seen['/auth/refresh'], 'ryzhub_refresh=r1');
    expect(secrets.values['refresh:https://hub.example'], 'r9');
    await client.post(Uri.parse('https://other.example/auth/refresh'));
    expect(seen['/auth/refresh'], isNull, reason: 'another server gets none');
    await client.post(Uri.parse('https://hub.example/auth/logout'));
    expect(secrets.values.containsKey('refresh:https://hub.example'), isFalse);
  });
}

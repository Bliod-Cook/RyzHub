import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:ryzhub/transport/event_stream.dart';

void main() {
  test('frames arrive as events and leave as posts, in order', () async {
    final server = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
    addTearDown(() => server.close(force: true));
    final posted = <List<int>>[];
    final streams = <HttpResponse>[];
    server.listen((request) async {
      if (request.method == 'GET') {
        final response = request.response
          ..headers.contentType = ContentType('text', 'event-stream')
          ..bufferOutput = false;
        streams.add(response);
        response.write('event: connection\ndata: abc\n\n');
        response.write(': keep-alive\n\n');
        response.write('data: ${base64Encode([1, 2, 3])}\n\n');
        await response.flush();
        return;
      }
      expect(request.uri.path, '/stream/abc');
      posted.add(await request.expand((chunk) => chunk).toList());
      request.response.statusCode = 204;
      await request.response.close();
    });
    final base = 'http://127.0.0.1:${server.port}';
    final channel = EventStreamChannel(
      Uri.parse('$base/stream?ticket=t&device_id=d'),
      post: (connection, frame) async {
        await http.post(Uri.parse('$base/stream/$connection'), body: frame);
      },
    );
    await channel.ready;
    expect(await channel.stream.first, [1, 2, 3]);
    channel.sink.add(Uint8List.fromList([9]));
    channel.sink.add([8]);
    for (var i = 0; i < 100 && posted.length < 2; i++) {
      await Future<void>.delayed(const Duration(milliseconds: 10));
    }
    expect(posted, [
      [9],
      [8],
    ]);
    // The server ending the stream ends the connection.
    await streams.single.close();
    await channel.sink.done;
  });
}

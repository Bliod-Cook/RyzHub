import 'dart:convert';

import 'package:http/http.dart' as http;

/// The events `GET uri` streams, as (event, data).
Stream<(String, String)> serverEvents(Uri uri) async* {
  final client = http.Client();
  try {
    final response = await client.send(
      http.Request('GET', uri)..headers['Accept'] = 'text/event-stream',
    );
    if (response.statusCode != 200) {
      throw http.ClientException('event stream ${response.statusCode}', uri);
    }
    var kind = 'message';
    final data = StringBuffer();
    await for (final line
        in response.stream
            .transform(utf8.decoder)
            .transform(const LineSplitter())) {
      if (line.isEmpty) {
        if (data.isNotEmpty) yield (kind, data.toString());
        kind = 'message';
        data.clear();
        continue;
      }
      // A comment keeps the connection alive.
      if (line.startsWith(':')) continue;
      final colon = line.indexOf(':');
      final field = colon < 0 ? line : line.substring(0, colon);
      var value = colon < 0 ? '' : line.substring(colon + 1);
      if (value.startsWith(' ')) value = value.substring(1);
      if (field == 'event') {
        kind = value;
      } else if (field == 'data') {
        if (data.isNotEmpty) data.write('\n');
        data.write(value);
      }
    }
  } finally {
    client.close();
  }
}

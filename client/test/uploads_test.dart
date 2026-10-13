import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';

void main() {
  test('a file read in any pieces goes up in parts of the same size', () async {
    final source = Stream<List<int>>.fromIterable([
      [1, 2, 3],
      [4],
      [5, 6, 7, 8, 9, 10, 11],
    ]);
    final parts = await byteChunks(source, 4).toList();
    expect(parts.map((p) => p.toList()), [
      [1, 2, 3, 4],
      [5, 6, 7, 8],
      [9, 10, 11],
    ]);
    expect(await byteChunks(Stream.value([1, 2]), 2).length, 1);
    expect(await byteChunks(const Stream.empty(), 2).length, 0);
  });
}

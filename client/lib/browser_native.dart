import 'dart:io';
import 'dart:typed_data';

import 'package:http/http.dart' as http;
import 'package:path_provider/path_provider.dart';
import 'package:url_launcher/url_launcher.dart';

/// Native builds sign in again on each start until the refresh token has a
/// home in the platform's secure storage.
const keepsSession = false;

http.Client sessionClient() => http.Client();

String sameSiteServer(String server) => server;

/// Saves [bytes] as [name] among the user's downloads and opens it.
Future<void> saveFile(String name, Uint8List bytes) async {
  final folder = await getDownloadsDirectory() ?? await getTemporaryDirectory();
  final file = File(
    '${folder.path}${Platform.pathSeparator}${name.replaceAll(RegExp(r'[/\\]'), '_')}',
  );
  await file.writeAsBytes(bytes);
  await launchUrl(Uri.file(file.path));
}

/// Files pasted into the window; native builds take files from the picker and
/// drops only. Returns the function that stops listening.
void Function() listenForPastedFiles(
  void Function(String name, Uint8List bytes) onFile,
) => () {};

/// Native builds hash with Dart; [open] is not read.
Future<String?> platformSha256(Stream<List<int>> Function() open) async => null;

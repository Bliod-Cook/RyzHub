import 'package:flutter/material.dart';

import 'app.dart';
import 'prefs.dart';
import 'rust_loader.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await initializeRust();
  await prefs.load();
  runApp(const RyzHubApp());
}

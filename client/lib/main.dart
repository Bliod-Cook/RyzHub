import 'package:flutter/material.dart';

import 'app.dart';
import 'prefs.dart';
import 'rust_loader.dart';

export 'prefs.dart' show prefs;
export 'ui/composer.dart' show Composer;
export 'ui/messages.dart' show MessageCard, QuestionCard;

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await initializeRust();
  await prefs.load();
  runApp(const RyzHubApp());
}

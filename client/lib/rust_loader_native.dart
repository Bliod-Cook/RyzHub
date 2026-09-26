import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart'
    show ExternalLibrary;

import 'rust/frb_generated.dart';

Future<void> initializeRust() async {
  const library = String.fromEnvironment('RYZHUB_RUST_LIBRARY');
  await RustLib.init(
    externalLibrary: library.isEmpty ? null : ExternalLibrary.open(library),
  );
}

#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# ANDROID_NDK_HOME must point to an installed NDK; cargo-ndk is required.
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 \
  -o client/android/app/src/main/jniLibs build -p ryzhub-bridge --release
cd client
flutter pub get
flutter build apk --release

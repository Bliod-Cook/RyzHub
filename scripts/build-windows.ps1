$ErrorActionPreference = 'Stop'
Set-Location "$PSScriptRoot/.."
cargo build -p ryzhub-bridge --release
if ($LASTEXITCODE -ne 0) { throw 'Rust build failed' }
Push-Location client
flutter pub get
flutter build windows --release
if ($LASTEXITCODE -ne 0) { throw 'Flutter build failed' }
Pop-Location
Copy-Item target/release/ryzhub_bridge.dll client/build/windows/x64/runner/Release/

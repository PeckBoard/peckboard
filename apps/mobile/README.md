# PeckBoard Mobile (iOS + Android)

Tauri 2 app that pairs a phone with one or more PeckBoard boxes and shows
each box's own web UI through a direct, end-to-end encrypted tunnel. Design:
`tmp-scratch/mobile-design.md`; this is Phase 1 (pair + open UI).

- Bundle id / package: `com.peckboard.app`; display name **PeckBoard**.
- Android: sideloaded APK. iOS: TestFlight (later). Desktop builds exist
  only for `cargo check` / UI iteration — never shipped.
- Releases use their own `mobile-X.Y.Z` tags (see Releasing the Mobile App).

## Architecture

```
apps/mobile/
├── index.html, src/            shell UI (TS + CSS, Vite): box list, pair, connect
├── src-tauri/                  Rust core
│   ├── src/link.rs             pasted/scanned text → PairingLink
│   ├── src/store.rs            paired boxes: metadata JSON + secrets in secure storage
│   ├── src/tunnel.rs           TunnelManager around peckboard_relay::tunnel::run_device
│   ├── src/nav.rs              loopback/gate URLs + WebView navigation allow-list
│   ├── src/commands.rs         shell-UI commands
│   ├── capabilities/           IPC grants (shell UI only; box UI gets nothing)
│   └── Info.ios.plist          merged into the generated iOS Info.plist
└── plugins/peckboard-native/   first-party Tauri plugin (Rust + Swift + Kotlin)
    ├── ios/Sources/…swift      Keychain, WKUIDelegate mic grant, lifecycle
    └── android/src/main/…      Keystore vault, WebChromeClient mic grant,
                                lifecycle, manifest perms, network security config
```

**Flow.** Tap a box → `connect_box` binds `127.0.0.1:<box port>` and starts
`run_device` (rendezvous → hole punch → QUIC), gated by a per-launch
`CookieGate`. Status events (`tunnel-status`) drive the connect screen; on
`connected` the WebView navigates to `http://127.0.0.1:<port>/__pbm/boot?k=<key>`,
the device-side gate sets an HttpOnly cookie and replaces itself with `/`,
and the box UI takes over. Every other local connection without the cookie
is dropped, so other apps on the phone can't use the port. Back (Android
button / iOS edge swipe) returns to the shell.

**Ports.** Each box gets a fixed port (41000, 41001, …) stored with its
pairing, because the web UI keeps its login in per-origin localStorage — a
changed port means signing in again. If the port is taken at connect time a
free one is used for that session and the UI says so.

**Lifecycle.** The tunnel and listener run only while the app is in the
foreground: the native plugin reports background/foreground (iOS
`didEnterBackground`/`willEnterForeground`, Android `onStop`/`onResume`);
background cancels `run_device` (listener dropped), foreground rebinds the
same port with the same gate key, so the box UI page, its login and the gate
cookie stay valid and its WebSocket reconnects on its own.

**Secrets.** The pairing link (the only credential) is stored via the
native plugin: iOS Keychain generic password,
`kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`, non-synchronizable;
Android: AES-256-GCM key in the Android Keystore wrapping the value in
app-private prefs; backups and device transfer are disabled. Non-secret
metadata (name, relay, port, a public key fingerprint for duplicate
detection) is `boxes.json` in the app data dir. Desktop uses a dev-only
0600 file.

**WebView hardening.**

- Navigation allow-list (`nav.rs`): the shell origin and
  `http://127.0.0.1:<active box port>` only; other http(s) links open in the
  system browser; everything else is blocked.
- Cleartext only to `127.0.0.1`: iOS `NSAllowsLocalNetworking` (no arbitrary
  loads); Android network security config (debug builds allow all cleartext
  so `tauri android dev` can reach Vite on your LAN).
- Mic: auto-granted for the loopback origin only (iOS 15+
  `requestMediaCapturePermissionFor`, Android `onPermissionRequest` once
  `RECORD_AUDIO` is held); denied for any other origin. Both wrap — not
  replace — wry's delegate/client so dialogs and file pickers keep working.
- Autoplay: Android `mediaPlaybackRequiresUserGesture = false`. iOS
  configuration flags are fixed at WKWebView creation; wry creates it with
  inline playback and autoplay enabled — verify on device.
- IPC: app commands are permission-gated (`build.rs` app manifest) and only
  `capabilities/shell*.json` grants them, to the local shell. The tunnelled
  box UI is a remote URL with no capability.

## Prerequisites

- Rust (stable), Node 20+, `npm install` in `apps/mobile`.
- Desktop (Linux): webkit2gtk-4.1 dev packages (present on the dev box).
- Android: JDK 17, Android SDK + NDK (`ANDROID_HOME`, `NDK_HOME`),
  `rustup target add aarch64-linux-android` (plus `armv7-linux-androideabi
x86_64-linux-android i686-linux-android` for emulators).
- iOS: macOS + Xcode, `rustup target add aarch64-apple-ios aarch64-apple-ios-sim`.

## Build and Run

```bash
cd apps/mobile
npm install

# Host checks (fast)
cargo check --manifest-path src-tauri/Cargo.toml
cargo test  --manifest-path src-tauri/Cargo.toml

# Shell UI in any browser with mocked IPC: npm run dev → http://localhost:1420/mock.html
# Desktop window for UI iteration (no secure storage, no lifecycle)
npx tauri dev

# Android — first time only, generates src-tauri/gen/android (commit it)
npx tauri android init
npx tauri android dev                 # emulator / USB device
npx tauri android build --apk --debug --target aarch64   # debug APK, unsigned release later

# iOS (on the Mac) — first time only, generates src-tauri/gen/apple (commit it)
npx tauri ios init
npx tauri ios dev
```

The plugin's Android manifest (permissions, `allowBackup=false`, data
extraction rules, network security config) is merged into the app manifest
by Gradle, and `Info.ios.plist` is merged by the Tauri CLI, so the generated
`gen/` projects need no hand edits. If manifest merging reports a conflict
on `allowBackup`/`networkSecurityConfig`, remove that attribute from
`gen/android/app/src/main/AndroidManifest.xml`.

Release builds, signing and publishing: see Releasing the Mobile App.

## Pair a Phone

1. On the box: Settings → Remote Access → **Add phone**. Use a fresh link
   per phone — reusing another device's link takes over its slot.
2. In the app: **Pair a box** → _Scan pairing QR code_, or paste the
   `peckboard://pair/…` link. Optionally name it.
3. The app connects right away and opens the box UI. Sign in once; the
   login is remembered for that box.

Connection states shown: _Finding your box…_, _Reconnecting…_, _Box offline_
(box not at the relay, or the pairing was revoked), _No direct path_ (both
NATs block hole punching — try another network or forward one UDP port on
the box's router), _Connection failed_. The app keeps retrying with backoff
while the screen is open.

**Deep links.** The app registers `peckboard://` (Tauri deep-link plugin).
Opening a `peckboard://pair/…` link — tapping it, or scanning the QR code
with the system camera — brings the app to a _Pair a box?_ screen that shows
the relay host; nothing is paired until the user taps **Pair**. A link that
launches the app is caught by `src/launch_url.rs` (iOS: tao drops the scene's
launch URLs). Test on the simulator with
`xcrun simctl openurl <udid> 'peckboard://pair/…'`.

**Debug log.** Debug builds write `log` + `tracing` output (relay punch and
QUIC traces included) to `debug.log` in the app data dir; on the simulator:
`$(xcrun simctl get_app_container <udid> com.peckboard.app data)/Library/Application Support/com.peckboard.app/debug.log`.

## Test

```bash
cargo test --manifest-path apps/mobile/src-tauri/Cargo.toml
```

Covers link parsing, the store round trip (secret kept out of the JSON),
port assignment/reuse, gate boot URL construction, the navigation
allow-list, event → status mapping, and pause/resume releasing and
rebinding the port. Device smoke tests (pair against
`peckboard-relay/examples/box_forward`, load `/`, WS connects) are manual
for now.

## Releasing the Mobile App

`.github/workflows/mobile-release.yml` ships the app on its own
`mobile-X.Y.Z` tags, independent of server releases.

1. Bump `version` in `src-tauri/tauri.conf.json` (keeps local builds in
   step; CI versions from the tag anyway), commit, push `main`.
2. `git tag -a mobile-0.1.0 -m "mobile-0.1.0" && git push origin mobile-0.1.0`.

CI stamps the version into `tauri.conf.json` on the runner: Android
`versionCode` = `X*1000000 + Y*1000 + Z` (must only ever grow), iOS build
number `<versionCode>.<run number>`. A manual run (Actions → Mobile Release →
Run workflow) picks `ios` / `android` / `both`; `upload: false` is a dry run
that only builds, signs and keeps the files as run artifacts.

**Secrets** (repo secrets, or the `ios-release` / `android-release`
environments):

- iOS: `APPLE_API_KEY_P8_B64` (App Store Connect API `.p8`, base64; Admin
  role so Xcode can create the cloud-managed distribution cert and profile),
  `APPLE_API_KEY_ID`, `APPLE_API_ISSUER_ID`, `APPLE_TEAM_ID`. The app record
  `com.peckboard.app` must already exist in App Store Connect.
- Android: `ANDROID_KEYSTORE_B64`, `ANDROID_KEYSTORE_PASSWORD`,
  `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD`, plus the repo **variable**
  `ANDROID_CERT_SHA256` (the release cert's SHA-256). Without the secrets
  the Android job is skipped with a notice; with a missing or different
  fingerprint it fails after printing the actual one. That key is the app's
  identity for sideloaded updates — never rotate it casually.

**iOS** goes to TestFlight (`xcrun altool --upload-app`). After App Store
Connect finishes processing, add the build to an internal testing group;
testers install it with the TestFlight app.

**Android** builds land on a GitHub release named after the tag (created as
not-latest, so server self-update is unaffected):
`peckboard-android-X.Y.Z.apk` + `.sha256`. On the phone, open the APK link,
allow the browser to install unknown apps when Android asks, and install.
To verify first: `sha256sum -c peckboard-android-X.Y.Z.apk.sha256` and
`apksigner verify --print-certs peckboard-android-X.Y.Z.apk` — the cert
SHA-256 must match the one in the release notes. Updates install over the
old app only when signed with the same key.

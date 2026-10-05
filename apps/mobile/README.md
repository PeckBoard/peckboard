# PeckBoard App (iOS, Android, macOS, Windows)

Tauri 2 app that pairs a phone or computer with one or more PeckBoard boxes
and shows each box's own web UI through a direct, end-to-end encrypted
tunnel. Design: `tmp-scratch/mobile-design.md`; this is Phase 1 (pair + open
UI).

- Bundle id / package: `com.peckboard.app`; display name **PeckBoard**.
- Android: sideloaded APK. iOS: TestFlight. macOS: universal DMG (Developer
  ID signed + notarized). Windows: x64 NSIS installer / MSI (unsigned).
  Linux desktop builds exist only for `cargo check` / UI iteration — not
  shipped.
- Releases use their own `mobile-X.Y.Z` tags (see Releasing the App).

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
│   ├── Info.ios.plist          merged into the generated iOS Info.plist
│   ├── Entitlements.ios.plist  iOS entitlements (associated domains for Universal Links)
│   ├── Info.macos.plist, Entitlements.macos.plist   macOS bundle (tauri.macos.conf.json)
│   └── tauri.windows.conf.json NSIS/MSI + WebView2 bootstrapper
└── plugins/peckboard-native/   first-party Tauri plugin (Rust + Swift + Kotlin)
    ├── src/desktop.rs          Keychain / Credential Manager (keyring), wake detection
    ├── ios/Sources/…swift      Keychain, WKUIDelegate mic grant, lifecycle
    └── android/src/main/…      Keystore vault, WebChromeClient mic grant,
                                lifecycle, manifest perms, network security config
```

**Flow.** Tap a box → `connect_box` binds `127.0.0.1:<box port>` and starts
`run_device` (rendezvous → hole punch → QUIC), gated by that box's own
`CookieGate`. Status events (`tunnel-status`) drive the connect screen; on
`connected` the WebView navigates to `http://127.0.0.1:<port>/__pbm/boot?k=<key>`,
the device-side gate sets an HttpOnly cookie and replaces itself with `/`,
and the box UI takes over. Every other local connection without the cookie
is dropped, so other apps on the phone can't use the port; the cookie is
removed before a request is forwarded, so the box never learns the key.
The app adds a **Boxes** button to every box page (top left, over the box
UI's logo; evaluated after each page load, see `src-tauri/src/boxes_button.js`)
that returns to the box list, on any box version; Back (Android button / iOS
edge swipe) does the same. Either way the shell stops the tunnel when it
loads. The button only navigates — box pages get no Tauri IPC.

**Ports.** Each box gets a fixed port (41000, 41001, …) stored with its
pairing, because the web UI keeps its login in per-origin localStorage — a
changed port means signing in again. If the port is taken at connect time a
free one is used for that session and the UI says so. A removed box's port is
**retired, never handed to another box** (`boxes.json` keeps a `next_port`
high-water mark; older files migrate to one past their highest port): the
WebView keeps website data per origin and no platform can clear exactly one
port's worth, so reuse would serve one box's login to another. Removing a
box clears what can be cleared (Android `WebStorage.deleteOrigin`), and
removing the **last** box wipes everything under `127.0.0.1` (all WebKit
data records for the host; Android storage, cookies, cache; plus Tauri's
`clear_all_browsing_data`). Only after all thousand ports have been used
once does allocation fall back to the lowest port no current box holds.

**Lifecycle.** The tunnel runs only while the app is in the foreground: the
native plugin reports background/foreground (iOS
`didEnterBackground`/`willEnterForeground`, Android `onStop`/`onResume`);
background cancels `run_device` but keeps the listener bound (nothing is
accepted), so another app on the phone can't take the port while the box
page is still alive and harvest its login token or gate cookie. Foreground
restarts the tunnel on that same listener with a **new** gate key; once it
is connected the WebView re-boots through the new key and lands back on the
page it was on (`&next=`). A key that leaked while backgrounded is dead.
The port is released only by disconnecting or switching box.

Desktop windows aren't suspended, but laptops sleep (and wake on another
network). The plugin's desktop half watches the wall clock; a jump of 30 s+
between 5 s ticks is reported as background + foreground, so the same
pause/resume path drops the stale tunnel and reconnects right away instead
of waiting out QUIC's idle timeout and the retry backoff.

**Secrets.** Credentials are stored via the native plugin: iOS Keychain
generic password, `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`,
non-synchronizable; Android: AES-256-GCM key in the Android Keystore
wrapping the value in app-private prefs; backups and device transfer are
disabled; macOS Keychain / Windows Credential Manager (`keyring` crate,
service `com.peckboard.app`). One item per kind and box (`store.rs`):
`box:<id>` the pairing link, `boxkey:<id>` this device's key seed while
enrolling, `box2:<id>` the enrolled credential (`peckboard-cred:2:…`: the
rendezvous secret, the device key and the pinned box key). Non-secret
metadata (name, relay, port, a public key fingerprint for duplicate
detection, `auth` = legacy / enrolling / enrolled, the box fingerprint) is
`boxes.json` in the app data dir. Linux (dev only) uses a 0600
`dev-secrets.json`; on macOS / Windows a `dev-secrets.json` left by an older
dev build is imported into the keychain once and deleted.

**Pairing v2** (`tmp-scratch/pairing-v2-design.md`). A v2 link
(`https://peckboard.com/pair#v=2&s=…&k=…&e=…`, or the same fields on
`peckboard://pair/<S>?…`) pins the box identity key and works once. Pairing
is an enrollment-only tunnel round (`TunnelManager::enroll`): the app
generates a device key (stored first, so a retry after a crash reuses it),
proves the link secret over ALPN `peckboard-tunnel/2` pinned to the box key,
enrolls the device key, and stores the credential the box grants
(`on_enrolled`, before the box is told). No box page is loaded. Right after,
a background "activate" connect lets the box retire the link secret; the
app then deletes `box:` and `boxkey:`. Pairings from before v2 keep working
as legacy (ALPN `/1`) and upgrade in place on their first connect to an
updated box. `credential()` picks what to connect with from what secure
storage holds, so a crash between the keychain write and the JSON save
heals itself.

**WebView hardening.**

- Navigation allow-list (`nav.rs`): this platform's shell origin
  (`tauri://localhost` on iOS/macOS, `http://tauri.localhost` on
  Android/Windows — never the other one) and
  `http://127.0.0.1:<active box port>` only; other http(s) links open in the
  system browser; everything else is blocked. Android history navigations
  (Back) skip the allow-list hook, so page starts are re-checked (`on_page_load`
  → back to the shell) and the WebView history is cleared whenever the shell
  finishes loading, so a box's pages can't be reached from another box.
- Cleartext only to `127.0.0.1`: iOS `NSAllowsLocalNetworking` (no arbitrary
  loads); Android network security config (debug builds allow all cleartext
  so `tauri android dev` can reach Vite on your LAN).
- Mic: granted only to the **active box's** origin
  (`http://127.0.0.1:<its port>`), from its main frame, and only after the
  user allowed it for that box — the first request shows "Allow <box> to use
  the microphone?" natively (iOS 15+ `requestMediaCapturePermissionFor`,
  Android `onPermissionRequest`) and the answer is remembered per box
  (`micAllowed` in `boxes.json`; the Rust core pushes the active box's policy
  to the plugin). Camera, other origins, sub-frames and no active box are
  denied. Both wrap — not replace — wry's delegate/client so dialogs and file
  pickers keep working. Desktop keeps the platform WebView's own prompt.
- Autoplay: Android `mediaPlaybackRequiresUserGesture = false`. iOS
  configuration flags are fixed at WKWebView creation; wry creates it with
  inline playback and autoplay enabled — verify on device.
- IPC: app commands are permission-gated (`build.rs` app manifest) and only
  `capabilities/shell*.json` grants them, to the local shell (`core:event`
  only, no other core APIs). The tunnelled box UI is a remote URL with no
  capability. Tauri 2 mobile has a single webview, so box pages can't be
  moved to a label of their own; instead every command also takes a
  `ShellProof` (`commands.rs`): the request must come from the `main`
  webview, carry the per-launch shell token (`X-PeckBoard-Shell`, defined on
  `window` by an initialization script only when the page is a shell origin)
  and, when the runtime can report it, show a shell URL. Tauri's own check
  judges the caller by the webview's current URL (on Android, the one seen at
  `onPageStarted`), which a box page racing a navigation to the shell could
  satisfy; the token closes that.

## Prerequisites

- Rust (stable), Node 20+, `npm install` in `apps/mobile`.
- Desktop: Linux needs webkit2gtk-4.1 dev packages (present on the dev box);
  macOS needs Xcode (universal builds: `rustup target add
aarch64-apple-darwin x86_64-apple-darwin`); Windows needs the MSVC build
  tools (WebView2 ships with Windows 11; the installer bootstraps it
  elsewhere).
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
# Desktop window (Linux: dev file store; macOS / Windows: keychain)
npx tauri dev

# Desktop release bundles, on the target OS (CI does this on tags)
npx tauri build --target universal-apple-darwin --bundles dmg   # macOS
npx tauri build --bundles nsis,msi                              # Windows

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

Release builds, signing and publishing: see Releasing the App.

## Pair a Phone

1. On the box: Settings → Remote Access → **Add phone**. Each link works
   once and expires after an hour; make one per device.
2. Scan the QR code with the system camera (or tap the link on the device):
   the app opens on a _Pair a box?_ screen showing the relay and the box
   fingerprint (`XXXX-XXXX-XXXX-XXXX`). Check it matches the one next to the
   QR code on the box, name the box, tap **Pair**. The app exchanges keys
   with the box and returns to the list ("Paired"); the box UI is **not**
   opened on its own after a deep link.
   Alternatively, in the app: **Pair a box** → _Scan pairing QR code_, or
   paste either link form; manual pairing opens the box right away.
3. Tap the box. Sign in once; the login is remembered for that box.

Pairing errors are spelled out: link already used by another device, link
expired, link needs a newer app, box key doesn't match the link ("don't
continue"), box offline ("…or the link expired" once past the expiry). A
link the box will never accept removes the half-paired box again; any
other failure leaves it as _Finishing pairing… tap to retry_.

Connection states shown: _Finding your box…_, _Reconnecting…_, _Box offline_
(box not at the relay, or the pairing was revoked), _Can't connect_ (neither
a direct path nor the encrypted relay fallback worked; no router setup is
ever required), _Connection failed_. The app keeps retrying with backoff
while the screen is open.

**Deep links.** Two forms open the app: `https://peckboard.com/pair#…`
(iOS Universal Links / Android App Links, `appLink: true` in
`tauri.conf.json`; verified against `docs/.well-known/` on peckboard.com)
and `peckboard://pair/…` (custom scheme, every platform). Either brings the
app to the confirm screen; nothing is paired until the user taps **Pair**,
and **Pair** pairs the link Rust holds for that prompt (`PairSlot`,
`confirm_pair`), never a string from the page. A second link opened while
one is being confirmed is dropped with a toast. The `/pair` page itself
(`docs/pair.html`) is static, sends nothing, and offers an "Open in the
PeckBoard app" button that builds the `peckboard://` form on tap (the
Windows / Linux / older-app path). A link that launches the app is caught
by `src/launch_url.rs` (iOS: tao drops the scene's launch URLs). Test on the
simulator with `xcrun simctl openurl <udid> 'https://peckboard.com/pair#…'`
(Universal Links need the AASA to verify at install time; the custom scheme
works regardless).

iOS needs the Associated Domains capability on the App ID and
`Entitlements.ios.plist` (`applinks:peckboard.com`) in the generated Xcode
project (`gen/apple/*_iOS/*_iOS.entitlements`); Android's App Link intent
filter is generated by the deep-link plugin into `gen/android`. Check both
after `tauri ios init` / `tauri android init`. After a deploy verify
`https://app-site-association.cdn-apple.com/a/v1/peckboard.com` returns the
AASA and `adb shell pm get-app-links com.peckboard.app` says `verified`.

On desktop the scheme is registered by the macOS bundle's Info.plist, the
Windows installer, and at runtime on Windows / Linux (`register_all`, which
also covers dev runs). A link clicked while the app is already running
starts a second process; `tauri-plugin-single-instance` (with its
`deep-link` feature) ends it and hands the link to the running window,
which comes to the front on the same confirm screen. Window size and
position persist across launches (`tauri-plugin-window-state`).

**Debug log.** Debug builds write `log` + `tracing` output (relay punch and
QUIC traces included) to `debug.log` in the app data dir; on the simulator:
`$(xcrun simctl get_app_container <udid> com.peckboard.app data)/Library/Application Support/com.peckboard.app/debug.log`.

## Test

```bash
cargo test --manifest-path apps/mobile/src-tauri/Cargo.toml
cargo test --manifest-path apps/mobile/plugins/peckboard-native/Cargo.toml
```

Covers link parsing (v1 / v2, https and `peckboard://` forms, surrounding
text, bad input, newer-version links), the deep-link prompt slot (a second
link is dropped while one shows), the store round trip (secrets kept out of
the JSON), the v2 credential lifecycle (stable device key across a retry,
`box2:` winning over a stale record, activation deleting the link, legacy
pairings loading as legacy and upgrading in place), port assignment (never
reused; old `boxes.json` files migrate), the shell IPC proof (`ShellProof`,
including a source guard that every command takes one), gate boot URL
construction, the navigation allow-list (this platform's shell origin
only), event → status mapping (pairing milestones, §5.6 error texts), and
pause/resume releasing and rebinding the port; the plugin test covers the
desktop `dev-secrets.json` → keychain import. Device smoke tests (pair
against `peckboard-relay/examples/box_forward`, load `/`, WS connects) are
manual for now.

## Releasing the App

`.github/workflows/mobile-release.yml` ships the app on its own
`mobile-X.Y.Z` tags, independent of server releases.

1. Bump `version` in `src-tauri/tauri.conf.json` (keeps local builds in
   step; CI versions from the tag anyway), commit, push `main`.
2. `git tag -a mobile-0.1.0 -m "mobile-0.1.0" && git push origin mobile-0.1.0`.
3. When the run has attached the assets, repoint every app link in
   `docs/downloads.md` (the site's Downloads page) at the new tag, refresh
   the "Current app release" line, and push `main`.

CI stamps the version into `tauri.conf.json` on the runner: Android
`versionCode` = `X*1000000 + Y*1000 + Z` (must only ever grow), iOS build
number `<versionCode>.<run number>`. A manual run (Actions → Mobile Release →
Run workflow) picks `all` / `mobile` / `desktop` / one platform;
`upload: false` is a dry run that only builds, signs and keeps the files as
run artifacts.

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
- macOS: `APPLE_DEVELOPER_ID_P12_B64` (Developer ID Application cert +
  private key, `.p12`, base64) and `APPLE_DEVELOPER_ID_P12_PASSWORD` — see
  Creating the Developer ID Certificate. Notarization reuses the iOS App
  Store Connect API key secrets. The macOS job runs in the `ios-release`
  environment, so put the two new secrets there or at repo level.
- Windows: none (unsigned).

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

**macOS** builds a universal (Apple silicon + Intel) DMG, macOS 11+:
`PeckBoard-X.Y.Z-macos-universal.dmg` + `.sha256` on the same release.
Signed with the Developer ID cert under the hardened runtime
(`Entitlements.macos.plist`: microphone only — outgoing network needs no
entitlement outside the App Sandbox), then notarized and stapled with the
App Store Connect API key. Without the Developer ID secrets the job warns
and ships an ad-hoc signed DMG that users must allow in System Settings →
Privacy & Security; without the API key it ships signed but un-notarized.

**Windows** builds an unsigned x64 NSIS installer
(`PeckBoard-X.Y.Z-windows-x64-setup.exe`) and MSI, per-user install, with
the WebView2 bootstrapper embedded. SmartScreen warns on first run (More
info → Run anyway) until a code-signing cert is added.

### Creating the Developer ID Certificate

Only the Apple Developer account holder can create one:

1. On a Mac, Keychain Access → Certificate Assistant → _Request a
   Certificate From a Certificate Authority…_ → your email, _Saved to
   disk_ → a `.certSigningRequest` file.
2. developer.apple.com → Certificates, Identifiers & Profiles →
   Certificates → **+** → **Developer ID Application** (G2 Sub-CA) →
   upload the CSR → download the `.cer` and double-click it to add it to
   the login keychain.
3. Keychain Access → My Certificates → right-click _Developer ID
   Application: …_ (expand it: the private key must be included) →
   Export → `.p12` with a strong password.
4. `base64 -i DeveloperID.p12 | pbcopy` → secret
   `APPLE_DEVELOPER_ID_P12_B64`; the password →
   `APPLE_DEVELOPER_ID_P12_PASSWORD`. Delete the exported `.p12` afterwards.

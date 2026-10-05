import Network
import Security
import Tauri
import UIKit
import WebKit

// Rust core is the only caller (no JS commands are exposed); see src/mobile.rs.

class KeyArgs: Decodable {
  let key: String
}

class SetArgs: Decodable {
  let key: String
  let value: String
}

class LifecycleArgs: Decodable {
  let channel: Channel
}

struct LifecycleMessage: Encodable {
  let state: String
}

class NetworkArgs: Decodable {
  let channel: Channel
}

struct NetworkMessage: Encodable {
  let detail: String
}

class ClearSiteDataArgs: Decodable {
  /// `http://127.0.0.1:<port>` of the removed box.
  let origin: String
  /// The last box is gone: wipe everything under the host.
  let hostWide: Bool
}

/// Who may capture the microphone (mirrors the Rust `MicPolicy`).
struct MicPolicy: Decodable {
  let origin: String
  let boxId: String
  let boxName: String
  /// nil: not asked yet.
  let allowed: Bool?
}

class MicPolicyArgs: Decodable {
  let policy: MicPolicy?
}

class MicDecisionArgs: Decodable {
  let channel: Channel
}

struct MicDecisionMessage: Encodable {
  let boxId: String
  let allowed: Bool
}

class PeckboardNativePlugin: Plugin {
  private var lifecycle: Channel?
  private var network: Channel?
  private var micDecisions: Channel?
  /// Guarded by `micLock`: written from the plugin's command queue, read by
  /// the UI delegate on the main thread. Never bounced through the main
  /// queue — the Rust caller may be blocking the main thread meanwhile.
  private var micPolicy: MicPolicy?
  private let micLock = NSLock()
  private var pathMonitor: NWPathMonitor?
  /// Interfaces + gateways of the last usable path; only touched on the
  /// monitor's queue.
  private var lastPath: String?
  private var uiDelegate: LoopbackUIDelegate?
  private var observers: [NSObjectProtocol] = []

  @objc public override func load(webview: WKWebView) {
    // Configuration flags (inline playback, autoplay) are copied when the
    // WKWebView is created, so they can't be changed here; wry creates it
    // with `allowsInlineMediaPlayback` on iOS and autoplay enabled. What can
    // be changed live is set here.
    // Edge swipe = Back: box UI history leads back to the shell (its first
    // entry), which then stops the tunnel. WebKit runs history navigations
    // through the navigation policy too, so the Rust allow-list still holds.
    webview.allowsBackForwardNavigationGestures = true
    // ` PeckBoardApp/<version>` tells the box UI it runs inside the app. The
    // shell loads first, so this lands before any box page is requested.
    if webview.customUserAgent == nil {
      webview.evaluateJavaScript("navigator.userAgent") { [weak webview] ua, _ in
        guard let webview = webview, let ua = ua as? String,
          !ua.contains("PeckBoardApp/")
        else { return }
        let version =
          Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
          ?? "0"
        webview.customUserAgent = "\(ua) PeckBoardApp/\(version)"
      }
    }
    // Wrap (not replace) wry's UI delegate: it implements alert/confirm/
    // prompt; we only take over media-capture decisions.
    let proxy = LoopbackUIDelegate(
      inner: webview.uiDelegate,
      policy: { [weak self] in self?.currentMicPolicy() },
      decided: { [weak self] boxId, allowed in
        try? self?.micDecisions?.send(MicDecisionMessage(boxId: boxId, allowed: allowed))
      })
    uiDelegate = proxy
    webview.uiDelegate = proxy

    let center = NotificationCenter.default
    observers.append(
      center.addObserver(
        forName: UIApplication.didEnterBackgroundNotification, object: nil, queue: .main
      ) { [weak self] _ in self?.emit("background") })
    observers.append(
      center.addObserver(
        forName: UIApplication.willEnterForegroundNotification, object: nil, queue: .main
      ) { [weak self] _ in self?.emit("foreground") })
  }

  deinit {
    observers.forEach { NotificationCenter.default.removeObserver($0) }
    pathMonitor?.cancel()
  }

  private func emit(_ state: String) {
    try? lifecycle?.send(LifecycleMessage(state: state))
  }

  @objc public func watchLifecycle(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(LifecycleArgs.self)
    lifecycle = args.channel
    invoke.resolve()
  }

  /// Reports a change of the default network path (Wi-Fi <-> cellular,
  /// another Wi-Fi), so the tunnel reconnects at once instead of waiting for
  /// its pings to time out. The Rust side debounces.
  @objc public func watchNetwork(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(NetworkArgs.self)
    network = args.channel
    if pathMonitor == nil {
      let monitor = NWPathMonitor()
      monitor.pathUpdateHandler = { [weak self] path in self?.pathUpdated(path) }
      monitor.start(queue: DispatchQueue(label: "com.peckboard.network-monitor"))
      pathMonitor = monitor
    }
    invoke.resolve()
  }

  /// Only a usable path whose interfaces or gateways differ from the last
  /// usable one counts: the first update (the path at start), unsatisfied
  /// interludes and flag-only updates (constrained, DNS) are ignored, so a
  /// flapping update stream doesn't keep tearing the tunnel down.
  private func pathUpdated(_ path: NWPath) {
    guard path.status == .satisfied else { return }
    let interfaces = path.availableInterfaces.map { "\($0.type):\($0.name)" }
    let key = (interfaces + path.gateways.map { "\($0)" }).joined(separator: ",")
    let last = lastPath
    lastPath = key
    guard let last = last, last != key else { return }
    try? network?.send(NetworkMessage(detail: interfaces.joined(separator: ",")))
  }

  private func currentMicPolicy() -> MicPolicy? {
    micLock.lock()
    defer { micLock.unlock() }
    return micPolicy
  }

  /// The active box's microphone policy (nil denies everything).
  @objc public func setMicPolicy(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(MicPolicyArgs.self)
    micLock.lock()
    micPolicy = args.policy
    micLock.unlock()
    invoke.resolve()
  }

  @objc public func watchMicDecision(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(MicDecisionArgs.self)
    micDecisions = args.channel
    invoke.resolve()
  }

  /// Website data of a removed box. WKWebsiteDataRecords are grouped by
  /// host, not origin, so one box's port can't be singled out: with
  /// `hostWide` (no box left) every record for `127.0.0.1` goes — all data
  /// types, cookies included; otherwise nothing can be removed here and the
  /// data stays orphaned behind a port the app never hands out again.
  @objc public func clearSiteData(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(ClearSiteDataArgs.self)
    guard args.hostWide, let host = URL(string: args.origin)?.host else {
      invoke.resolve(["cleared": false])
      return
    }
    DispatchQueue.main.async {
      let store = WKWebsiteDataStore.default()
      let types = WKWebsiteDataStore.allWebsiteDataTypes()
      store.fetchDataRecords(ofTypes: types) { records in
        let mine = records.filter { $0.displayName == host }
        store.removeData(ofTypes: types, for: mine) {
          invoke.resolve(["cleared": !mine.isEmpty])
        }
      }
    }
  }

  @objc public func secretGet(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(KeyArgs.self)
    switch Keychain.get(args.key) {
    case .success(let value):
      if let value = value {
        invoke.resolve(["value": value])
      } else {
        invoke.resolve()
      }
    case .failure(let status):
      invoke.reject("keychain read failed (\(status))")
    }
  }

  @objc public func secretSet(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(SetArgs.self)
    let status = Keychain.set(args.key, args.value)
    if status == errSecSuccess {
      invoke.resolve()
    } else {
      invoke.reject("keychain write failed (\(status))")
    }
  }

  @objc public func secretDelete(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(KeyArgs.self)
    let status = Keychain.delete(args.key)
    if status == errSecSuccess || status == errSecItemNotFound {
      invoke.resolve()
    } else {
      invoke.reject("keychain delete failed (\(status))")
    }
  }
}

/// Generic-password items, this device only, never synced to iCloud.
enum Keychain {
  static let service = "com.peckboard.app.pairing"

  private static func base(_ key: String) -> [String: Any] {
    [
      kSecClass as String: kSecClassGenericPassword,
      kSecAttrService as String: service,
      kSecAttrAccount as String: key,
      kSecAttrSynchronizable as String: kCFBooleanFalse!,
    ]
  }

  static func get(_ key: String) -> Result<String?, KeychainError> {
    var q = base(key)
    q[kSecReturnData as String] = kCFBooleanTrue
    q[kSecMatchLimit as String] = kSecMatchLimitOne
    var out: AnyObject?
    let status = SecItemCopyMatching(q as CFDictionary, &out)
    if status == errSecItemNotFound { return .success(nil) }
    guard status == errSecSuccess, let data = out as? Data else {
      return .failure(KeychainError(status: status))
    }
    return .success(String(data: data, encoding: .utf8))
  }

  static func set(_ key: String, _ value: String) -> OSStatus {
    let data = Data(value.utf8)
    let attrs: [String: Any] = [
      kSecValueData as String: data,
      kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
    ]
    let status = SecItemUpdate(base(key) as CFDictionary, attrs as CFDictionary)
    if status != errSecItemNotFound { return status }
    var add = base(key)
    add.merge(attrs) { _, new in new }
    return SecItemAdd(add as CFDictionary, nil)
  }

  static func delete(_ key: String) -> OSStatus {
    SecItemDelete(base(key) as CFDictionary)
  }
}

struct KeychainError: Error, CustomStringConvertible {
  let status: OSStatus
  var description: String { "OSStatus \(status)" }
}

/// Forwards every WKUIDelegate call to wry's delegate except media capture.
/// The microphone is granted only to the active box's loopback origin
/// (`policy().origin`), from its main frame, and only after the user allowed
/// it for that box: the first request shows "Allow <box> to use the
/// microphone?" and the answer is remembered with the box (`decided`).
/// Camera, other origins, sub-frames and no active box are denied. The
/// OS-level microphone permission (NSMicrophoneUsageDescription) is still
/// asked once by the system.
class LoopbackUIDelegate: NSObject, WKUIDelegate {
  private let inner: WKUIDelegate?
  private let policy: () -> MicPolicy?
  private let decided: (String, Bool) -> Void

  init(inner: WKUIDelegate?, policy: @escaping () -> MicPolicy?, decided: @escaping (String, Bool) -> Void) {
    self.inner = inner
    self.policy = policy
    self.decided = decided
  }

  override func responds(to aSelector: Selector!) -> Bool {
    super.responds(to: aSelector) || (inner?.responds(to: aSelector) ?? false)
  }

  override func forwardingTarget(for aSelector: Selector!) -> Any? {
    if let inner = inner, inner.responds(to: aSelector) { return inner }
    return super.forwardingTarget(for: aSelector)
  }

  /// `scheme://host[:port]`, like the Rust side's `nav::origin`.
  private static func originString(_ origin: WKSecurityOrigin) -> String {
    origin.port == 0 ? "\(origin.protocol)://\(origin.host)" : "\(origin.protocol)://\(origin.host):\(origin.port)"
  }

  private static func originString(_ url: URL) -> String? {
    guard let scheme = url.scheme, let host = url.host else { return nil }
    if let port = url.port { return "\(scheme)://\(host):\(port)" }
    return "\(scheme)://\(host)"
  }

  @available(iOS 15.0, *)
  func webView(
    _ webView: WKWebView,
    requestMediaCapturePermissionFor origin: WKSecurityOrigin,
    initiatedByFrame frame: WKFrameInfo,
    type: WKMediaCaptureType,
    decisionHandler: @escaping (WKPermissionDecision) -> Void
  ) {
    guard type == .microphone, frame.isMainFrame, let policy = policy(),
      Self.originString(origin) == policy.origin,
      let page = webView.url, Self.originString(page) == policy.origin
    else {
      return decisionHandler(.deny)
    }
    switch policy.allowed {
    case .some(true):
      decisionHandler(.grant)
    case .some(false):
      decisionHandler(.deny)
    case .none:
      guard let presenter = Self.presenter(for: webView) else {
        return decisionHandler(.deny)
      }
      let decided = self.decided
      let alert = UIAlertController(
        title: "Microphone",
        message: "Allow “\(policy.boxName)” to use the microphone?",
        preferredStyle: .alert)
      alert.addAction(
        UIAlertAction(title: "Don’t Allow", style: .cancel) { _ in
          decided(policy.boxId, false)
          decisionHandler(.deny)
        })
      alert.addAction(
        UIAlertAction(title: "Allow", style: .default) { _ in
          decided(policy.boxId, true)
          decisionHandler(.grant)
        })
      presenter.present(alert, animated: true)
    }
  }

  private static func presenter(for webView: WKWebView) -> UIViewController? {
    var vc = webView.window?.rootViewController
    while let next = vc?.presentedViewController { vc = next }
    return vc
  }
}

@_cdecl("init_plugin_peckboard_native")
func initPlugin() -> Plugin {
  return PeckboardNativePlugin()
}

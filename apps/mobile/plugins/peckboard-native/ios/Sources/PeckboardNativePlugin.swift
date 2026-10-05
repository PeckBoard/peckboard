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

class PeckboardNativePlugin: Plugin {
  private var lifecycle: Channel?
  private var network: Channel?
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
    // entry), which then stops the tunnel.
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
    let proxy = LoopbackUIDelegate(inner: webview.uiDelegate)
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

/// Forwards every WKUIDelegate call to wry's delegate except media capture:
/// the microphone is granted without a prompt for the loopback origin (the
/// tunnelled box UI), everything else is denied. The OS-level microphone
/// permission (NSMicrophoneUsageDescription) is still asked once.
class LoopbackUIDelegate: NSObject, WKUIDelegate {
  private let inner: WKUIDelegate?

  init(inner: WKUIDelegate?) {
    self.inner = inner
  }

  override func responds(to aSelector: Selector!) -> Bool {
    super.responds(to: aSelector) || (inner?.responds(to: aSelector) ?? false)
  }

  override func forwardingTarget(for aSelector: Selector!) -> Any? {
    if let inner = inner, inner.responds(to: aSelector) { return inner }
    return super.forwardingTarget(for: aSelector)
  }

  @available(iOS 15.0, *)
  func webView(
    _ webView: WKWebView,
    requestMediaCapturePermissionFor origin: WKSecurityOrigin,
    initiatedByFrame frame: WKFrameInfo,
    type: WKMediaCaptureType,
    decisionHandler: @escaping (WKPermissionDecision) -> Void
  ) {
    let loopback = origin.protocol == "http" && origin.host == "127.0.0.1"
    guard loopback else { return decisionHandler(.deny) }
    decisionHandler(type == .microphone ? .grant : .prompt)
  }
}

@_cdecl("init_plugin_peckboard_native")
func initPlugin() -> Plugin {
  return PeckboardNativePlugin()
}

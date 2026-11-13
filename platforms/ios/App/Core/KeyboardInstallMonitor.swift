import UIKit
import Combine

/// 判断键盘有没有被加进系统的键盘列表。
///
/// iOS 不让 App 直接问「某个键盘开了没」。这里读系统用户默认设置里的 AppleKeyboards：
/// 它是「已添加键盘」的列表，第三方键盘的条目里带着扩展的 bundle id，用包含匹配。
/// 只能知道「已添加」，不能知道「正在使用」，也看不出「允许完全访问」有没有开。
enum KeyboardInstallDetector {
    static func isAdded(extensionID: String, enabledKeyboards: [String]) -> Bool {
        enabledKeyboards.contains { $0.contains(extensionID) }
    }
}

@MainActor
final class KeyboardInstallMonitor: ObservableObject {
    @Published private(set) var isAdded = false

    static let extensionID = "com.zengji.aipinyin.keyboard"

    private var observer: NSObjectProtocol?

    init() {
        refresh()
        observer = NotificationCenter.default.addObserver(
            forName: UITextInputMode.currentInputModeDidChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.refresh() }
        }
    }

    func refresh() {
        let enabled = UserDefaults.standard.array(forKey: "AppleKeyboards") as? [String] ?? []
        isAdded = KeyboardInstallDetector.isAdded(extensionID: Self.extensionID, enabledKeyboards: enabled)
    }
}

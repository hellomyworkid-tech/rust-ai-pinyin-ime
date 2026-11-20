import Darwin
import Foundation
import UIKit
import os

/// 可取消的异步操作句柄（供跨动态库边界取消端侧模型推理任务）
@objc(AIPinyinAuxCancellable)
public protocol AuxCancellable: AnyObject {
    func cancel()
}

/// 快捷设置面板里的原生 SwiftUI `Link` 覆盖层宿主接口：
/// 让键盘主二进制完全不链接 `SwiftUI.framework`，仅在用户点开设置面板时按需从 `AIPinyinAux.framework` 创建。
@objc(AIPinyinAuxLinkHostControlling)
public protocol AuxLinkHostControlling: AnyObject {
    @MainActor var viewController: UIViewController { get }
    @MainActor func update(destination: URL,
                           isInteractive: Bool,
                           accessibilityLabel: String,
                           accessibilityValue: String,
                           onPressChanged: @escaping (Bool) -> Void)
}

/// `AIPinyinAux.framework` 提供的重型系统框架桥接入口（封装 `FoundationModels` 与 `SwiftUI`）
@objc(AIPinyinAuxBridgeProtocol)
public protocol AuxBridgeProviding: AnyObject {
    init()
    /// 同步探测 Apple 端侧模型是否可用（调用方负责放到后台任务执行）
    func isAppleModelAvailable() -> Bool
    /// 创建并预热一个 Apple 端侧模型会话对象；不可用时返回 nil
    func prewarmAppleSession(instructions: String) -> AnyObject?
    /// 执行一次引导式候选选择，返回可取消句柄
    func chooseWithAppleModel(prompt: String,
                              options: [String],
                              instructions: String,
                              prewarmedSession: AnyObject?,
                              completion: @escaping @Sendable (String?, NSError?) -> Void) -> AuxCancellable
    /// 创建包裹原生 SwiftUI `Link` 的透明子控制器
    @MainActor
    func makeLinkHost(destination: URL,
                      isInteractive: Bool,
                      accessibilityLabel: String,
                      accessibilityValue: String,
                      onPressChanged: @escaping (Bool) -> Void) -> AuxLinkHostControlling
}

/// 按需加载 `AIPinyin.app/Frameworks/AIPinyinAux.framework` 并缓存桥接单例。
/// 冷启动呼出键盘的首帧路径绝不调用它，因此 `dyld` 启动期无需映射或初始化 `llama.cpp`、`FoundationModels`、`SwiftUI`。
enum AuxBridgeLoader {
    private static let lock = OSAllocatedUnfairLock<AuxBridgeProviding?>(initialState: nil)

    static func shared() -> AuxBridgeProviding? {
        if let existing = lock.withLock({ $0 }) {
            return existing
        }
        let loaded = instantiateBridge()
        return lock.withLock { state in
            if let existing = state {
                return existing
            }
            state = loaded
            return loaded
        }
    }

    private static func instantiateBridge() -> AuxBridgeProviding? {
        if let cls = NSClassFromString("AIPinyinAuxBridge") as? AuxBridgeProviding.Type {
            return cls.init()
        }
        loadFrameworkIfNeeded()
        guard let cls = NSClassFromString("AIPinyinAuxBridge") as? AuxBridgeProviding.Type else {
            return nil
        }
        return cls.init()
    }

    private static func loadFrameworkIfNeeded() {
        let mainURL = Bundle.main.bundleURL
        var candidates: [URL] = [
            // 键盘扩展位于 AIPinyin.app/PlugIns/AIPinyinKeyboard.appex → ../../Frameworks/AIPinyinAux.framework
            mainURL.deletingLastPathComponent().deletingLastPathComponent()
                .appendingPathComponent("Frameworks/AIPinyinAux.framework", isDirectory: true),
        ]
        if let privateFrameworks = Bundle.main.privateFrameworksURL {
            candidates.append(privateFrameworks.appendingPathComponent("AIPinyinAux.framework", isDirectory: true))
        }
        for fwURL in candidates {
            if let bundle = Bundle(url: fwURL), bundle.load() {
                return
            }
        }
        _ = dlopen("@rpath/AIPinyinAux.framework/AIPinyinAux", RTLD_LAZY | RTLD_GLOBAL)
    }
}

import Foundation
import os

/// Apple 端侧模型（FoundationModels）选词。
///
/// 真机实测（iPhone 16 Pro Max，iOS 27.2）：键盘扩展里不开完全访问也能用，多占约 4MB，
/// 每次 210–260ms，首次 0.6–1.1s。用引导生成（DynamicGenerationSchema anyOf）把答案限制在候选之内，
/// 贪心采样。只用 SystemLanguageModel；PrivateCloudComputeLanguageModel 需要本项目没有的权限，
/// 调用会直接杀掉进程，绝不能用。
///
/// 注意：
/// 1. `FoundationModels.framework` 封装在按需加载的 `AIPinyinAux.framework` 中，键盘主二进制冷启动时完全不链接它；
/// 2. 长时间不用键盘后，系统后台守护进程（`modelcatalogd` / `generativeexperiencesd`）可能处于休眠态，
///    可用性探测与会话 `prewarm()` 一律放到后台低优先级任务中完成，主线程只读锁内缓存。
@MainActor
final class AppleChoiceModel: ChoiceModel {
    nonisolated static let instructions = "你是中文拼音输入法的选词助手。根据上文和拼音，从候选里选出用户最可能想输入的那一项，只输出这一项。"

    private final class SendableSessionBox: @unchecked Sendable {
        let raw: AnyObject
        init(_ raw: AnyObject) { self.raw = raw }
    }

    private final class CancellableHolder: @unchecked Sendable {
        private let lock = OSAllocatedUnfairLock<(cancelled: Bool, task: AuxCancellable?)>(
            initialState: (false, nil)
        )

        func install(_ task: AuxCancellable) {
            let shouldCancelNow = lock.withLock { state -> Bool in
                if state.cancelled {
                    return true
                }
                state.task = task
                return false
            }
            if shouldCancelNow {
                task.cancel()
            }
        }

        func cancel() {
            let active = lock.withLock { state -> AuxCancellable? in
                state.cancelled = true
                let current = state.task
                state.task = nil
                return current
            }
            active?.cancel()
        }
    }

    /// 预热过、还没用过的会话句柄。每次选词都用新会话，会话之间互不影响
    private var prewarmedSession: AnyObject?
    private var prewarming = false
    private let availabilityCache = OSAllocatedUnfairLock<Bool?>(initialState: nil)

    var knownAvailability: Bool? {
        availabilityCache.withLock { $0 }
    }

    var isAvailable: Bool {
        if let cached = availabilityCache.withLock({ $0 }) {
            return cached
        }
        let available = Self.querySystemAvailability()
        availabilityCache.withLock { $0 = available }
        return available
    }

    func refreshAvailability() async -> Bool {
        let cache = availabilityCache
        return await Task.detached(priority: .utility) {
            let available = Self.querySystemAvailability()
            cache.withLock { $0 = available }
            return available
        }.value
    }

    func prewarm() {
        guard prewarmedSession == nil, !prewarming else { return }
        if availabilityCache.withLock({ $0 }) == false { return }
        prewarming = true
        let cache = availabilityCache
        Task { @MainActor [weak self] in
            let readyBox: SendableSessionBox? = await Task.detached(priority: .utility) {
                guard let bridge = AuxBridgeLoader.shared() else {
                    cache.withLock { $0 = false }
                    return nil
                }
                let available = bridge.isAppleModelAvailable()
                cache.withLock { $0 = available }
                guard available, let session = bridge.prewarmAppleSession(instructions: Self.instructions) else {
                    return nil
                }
                return SendableSessionBox(session)
            }.value
            guard let self else { return }
            self.prewarming = false
            if let readyBox, self.prewarmedSession == nil {
                self.prewarmedSession = readyBox.raw
            }
        }
    }

    func choose(_ request: IMEChoiceRequest) async throws -> String? {
        let available: Bool
        if let cached = knownAvailability {
            available = cached
        } else {
            available = await refreshAvailability()
        }
        guard available else { return nil }
        var seen = Set<String>()
        let options = request.candidates.filter { !$0.isEmpty && seen.insert($0).inserted }
        guard options.count >= 2 else { return options.first }
        guard let bridge = AuxBridgeLoader.shared() else { return nil }

        let session = takeSession()
        let promptText = Self.prompt(for: request, options: options)
        let holder = CancellableHolder()

        let answer: String? = try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                let cancellable = bridge.chooseWithAppleModel(
                    prompt: promptText,
                    options: options,
                    instructions: Self.instructions,
                    prewarmedSession: session
                ) { chosen, nsError in
                    if let nsError {
                        continuation.resume(throwing: nsError)
                    } else {
                        continuation.resume(returning: chosen)
                    }
                }
                holder.install(cancellable)
            }
        } onCancel: {
            holder.cancel()
        }
        try Task.checkCancellation()
        return answer
    }

    nonisolated private static func querySystemAvailability() -> Bool {
        AuxBridgeLoader.shared()?.isAppleModelAvailable() ?? false
    }

    static func prompt(for request: IMEChoiceRequest, options: [String]) -> String {
        "上文：\(request.context)\n拼音：\(request.pinyin)\n候选：\(options.joined(separator: " / "))"
    }

    private func takeSession() -> AnyObject? {
        if let session = prewarmedSession {
            prewarmedSession = nil
            return session
        }
        return nil
    }
}

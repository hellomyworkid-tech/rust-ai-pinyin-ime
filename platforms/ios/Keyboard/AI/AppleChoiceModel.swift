import Foundation
import FoundationModels
import os

/// Apple 端侧模型（FoundationModels）选词。
///
/// 真机实测（iPhone 16 Pro Max，iOS 27.2）：键盘扩展里不开完全访问也能用，多占约 4MB，
/// 每次 210–260ms，首次 0.6–1.1s。用引导生成（DynamicGenerationSchema anyOf）把答案限制在候选之内，
/// 贪心采样。只用 SystemLanguageModel；PrivateCloudComputeLanguageModel 需要本项目没有的权限，
/// 调用会直接杀掉进程，绝不能用。
///
/// 注意：长时间不用键盘后，系统后台守护进程（`modelcatalogd` / `generativeexperiencesd`）可能处于休眠态，
/// 在主线程直接同步读取 `SystemLanguageModel.default.availability` 或创建/预热会话会通过 XPC 阻塞主线程 0.6–1.5s。
/// 因此可用性探测与会话 `prewarm()` 一律放到后台低优先级任务中完成，主线程只读锁内缓存。
@MainActor
final class AppleChoiceModel: ChoiceModel {
    nonisolated static let instructions = "你是中文拼音输入法的选词助手。根据上文和拼音，从候选里选出用户最可能想输入的那一项，只输出这一项。"

    /// 预热过、还没用过的会话。每次选词都用新会话，会话之间互不影响
    private var prewarmedSession: LanguageModelSession?
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
            let readySession: LanguageModelSession? = await Task.detached(priority: .utility) {
                let available = Self.querySystemAvailability()
                cache.withLock { $0 = available }
                guard available else { return nil }
                let session = LanguageModelSession(model: SystemLanguageModel.default, instructions: Self.instructions)
                session.prewarm()
                return session
            }.value
            guard let self else { return }
            self.prewarming = false
            if let readySession, self.prewarmedSession == nil {
                self.prewarmedSession = readySession
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

        let session = takeSession()
        let schema = try GenerationSchema(
            root: DynamicGenerationSchema(name: "Choice", anyOf: options), dependencies: [])
        // 用 String 提示词 + schema 的这个重载，避免和 Generable 重载产生歧义
        let response = try await session.respond(
            to: Self.prompt(for: request, options: options),
            schema: schema,
            includeSchemaInPrompt: true,
            options: GenerationOptions(samplingMode: .greedy))
        try Task.checkCancellation()
        return try response.content.value(String.self)
    }

    nonisolated private static func querySystemAvailability() -> Bool {
        if case .available = SystemLanguageModel.default.availability { return true }
        return false
    }

    static func prompt(for request: IMEChoiceRequest, options: [String]) -> String {
        "上文：\(request.context)\n拼音：\(request.pinyin)\n候选：\(options.joined(separator: " / "))"
    }

    private func makeSession() -> LanguageModelSession {
        LanguageModelSession(model: SystemLanguageModel.default, instructions: Self.instructions)
    }

    private func takeSession() -> LanguageModelSession {
        if let session = prewarmedSession {
            prewarmedSession = nil
            return session
        }
        return makeSession()
    }
}

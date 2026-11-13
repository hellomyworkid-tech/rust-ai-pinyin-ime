import Foundation
import FoundationModels

/// Apple 端侧模型（FoundationModels）选词。
///
/// 真机实测（iPhone 16 Pro Max，iOS 27.2）：键盘扩展里不开完全访问也能用，多占约 4MB，
/// 每次 210–260ms，首次 0.6–1.1s。用引导生成（DynamicGenerationSchema anyOf）把答案限制在候选之内，
/// 贪心采样。只用 SystemLanguageModel；PrivateCloudComputeLanguageModel 需要本项目没有的权限，
/// 调用会直接杀掉进程，绝不能用。
@MainActor
final class AppleChoiceModel: ChoiceModel {
    static let instructions = "你是中文拼音输入法的选词助手。根据上文和拼音，从候选里选出用户最可能想输入的那一项，只输出这一项。"

    /// 预热过、还没用过的会话。每次选词都用新会话，会话之间互不影响
    private var prewarmedSession: LanguageModelSession?

    var isAvailable: Bool {
        if case .available = SystemLanguageModel.default.availability { return true }
        return false
    }

    func prewarm() {
        guard isAvailable, prewarmedSession == nil else { return }
        let session = makeSession()
        session.prewarm()
        prewarmedSession = session
    }

    func choose(_ request: IMEChoiceRequest) async throws -> String? {
        guard isAvailable else { return nil }
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

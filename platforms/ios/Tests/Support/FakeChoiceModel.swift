import Foundation

/// 假的选词模型，替换 Apple 端侧模型
@MainActor
final class FakeChoiceModel: ChoiceModel {
    enum Behavior {
        /// 立即回答
        case answer(String?)
        /// 抛错
        case fail
        /// 等测试调用 resolve 再回答
        case wait
        /// 一直不答（直到被取消）
        case hang
    }

    struct Failure: Error {}

    var isAvailable = true
    var behavior: Behavior = .answer(nil)
    private(set) var prewarmCount = 0
    private(set) var requests: [IMEChoiceRequest] = []
    private var waiting: [UInt64: CheckedContinuation<String?, Error>] = [:]

    func prewarm() {
        prewarmCount += 1
    }

    func choose(_ request: IMEChoiceRequest) async throws -> String? {
        requests.append(request)
        switch behavior {
        case .answer(let text):
            return text
        case .fail:
            throw Failure()
        case .wait:
            return try await withCheckedThrowingContinuation { waiting[request.id] = $0 }
        case .hang:
            try await Task.sleep(for: .seconds(30))
            return nil
        }
    }

    func resolve(_ id: UInt64, _ answer: String?) {
        waiting.removeValue(forKey: id)?.resume(returning: answer)
    }
}

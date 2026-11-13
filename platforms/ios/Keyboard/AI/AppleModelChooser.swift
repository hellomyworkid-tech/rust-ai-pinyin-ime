import Foundation

/// 回答引擎的选词请求（engine.onChoiceRequest）。
///
/// - 同一时间只跑一个请求；新请求到达时，旧的立即作废（交回 nil）并取消；
/// - 请求先等一小段（debounce）再交给模型：连续打字时每个字母都会来一个请求，
///   等不到这段时间就被下一个取代的请求根本不会惊动模型，省电也省发热；
/// - 超时（默认 1.5 秒，从收到请求算起）交回 nil；
/// - 模型不可用、出错、答案不在候选里，都交回 nil，不会崩溃；
/// - 引擎作废的请求（cancel(id:)）立即中止，不交回。
/// 除了引擎作废的，每个请求恰好交回一次结果。
@MainActor
final class AppleModelChooser {
    nonisolated static let defaultTimeout: TimeInterval = 1.5
    /// 键盘里用的等待时间：比快速打字时两次按键的间隔略短
    nonisolated static let keyboardDebounce: TimeInterval = 0.12

    private let model: ChoiceModel
    let timeout: TimeInterval
    let debounce: TimeInterval
    /// 交回结果，通常接到 engine.deliverChoice
    var deliver: ((UInt64, String?) -> Void)?

    private struct Running {
        let id: UInt64
        let work: Task<Void, Never>
        let timer: Task<Void, Never>
    }

    private var running: Running?
    /// 作废通知可能先于同一编号的请求到达：记下来，那个请求到了就不再交给模型
    private var cancelledAhead: UInt64?

    init(model: ChoiceModel, timeout: TimeInterval = AppleModelChooser.defaultTimeout, debounce: TimeInterval = 0) {
        self.model = model
        self.timeout = timeout
        self.debounce = debounce
    }

    var isAvailable: Bool { model.isAvailable }
    var isBusy: Bool { running != nil }

    func prewarm() {
        guard model.isAvailable else { return }
        model.prewarm()
    }

    func request(_ request: IMEChoiceRequest) {
        cancel()
        if request.id == cancelledAhead {
            // 引擎已经不等它了，不必问模型，也不必交回
            cancelledAhead = nil
            return
        }
        guard model.isAvailable, request.candidates.count >= 2 else {
            deliver?(request.id, nil)
            return
        }
        let id = request.id
        let model = self.model
        let delay = debounce
        let work = Task { [weak self] in
            if delay > 0 {
                try? await Task.sleep(for: .seconds(delay))
                // 等待期间被新请求取代或取消：结果已经由 cancel() 交回，这里直接结束
                guard !Task.isCancelled else { return }
            }
            let answer: String?
            do {
                answer = try await model.choose(request)
            } catch {
                answer = nil
            }
            let valid = answer.flatMap { request.candidates.contains($0) ? $0 : nil }
            self?.finish(id, valid)
        }
        let seconds = timeout
        let timer = Task { [weak self] in
            try? await Task.sleep(for: .seconds(seconds))
            guard !Task.isCancelled else { return }
            self?.finish(id, nil)
        }
        running = Running(id: id, work: work, timer: timer)
    }

    /// 放弃正在跑的请求（新请求到达、键盘收起）
    func cancel() {
        guard let current = running else { return }
        running = nil
        current.work.cancel()
        current.timer.cancel()
        deliver?(current.id, nil)
    }

    /// 换了引擎：放弃正在跑的请求，忘掉为旧引擎记下的「已作废的编号」（新引擎的编号从头算，不能误伤）
    func reset() {
        cancel()
        cancelledAhead = nil
    }

    /// 引擎作废了编号 id 的请求（输入结束、被新请求取代、换了后端）：正在跑的就是它的话立即中止。
    /// 不交回结果——引擎已经不等它了。别的编号的请求不受影响
    func cancel(id: UInt64) {
        guard let current = running, current.id == id else {
            // 不是正在跑的那个：多半是还没到的请求（也可能早就结束了，记下也无害，编号不会再出现）
            cancelledAhead = id
            return
        }
        running = nil
        current.work.cancel()
        current.timer.cancel()
    }

    private func finish(_ id: UInt64, _ answer: String?) {
        guard let current = running, current.id == id else { return }
        running = nil
        current.work.cancel()
        current.timer.cancel()
        deliver?(id, answer)
    }
}

import Foundation

/// 手动推进的调度器，替换 MockEngine 里的 DispatchQueue.main.asyncAfter
final class ManualScheduler {
    private(set) var pending: [(delay: TimeInterval, work: () -> Void)] = []

    var schedule: MockEngine.Scheduler {
        { [unowned self] delay, work in self.pending.append((delay, work)) }
    }

    /// 执行所有已排队的任务（执行中新排进来的也一起跑完）
    func runAll() {
        while !pending.isEmpty {
            let batch = pending
            pending.removeAll()
            batch.forEach { $0.work() }
        }
    }

    /// 只执行延迟不超过 maxDelay 的任务
    func run(upTo maxDelay: TimeInterval) {
        let ready = pending.filter { $0.delay <= maxDelay }
        pending.removeAll { $0.delay <= maxDelay }
        ready.forEach { $0.work() }
    }
}

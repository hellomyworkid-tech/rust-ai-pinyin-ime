import Foundation

/// 上档键：单击一次只管下一个字母，双击锁定大写，锁定时再点一下解除
struct ShiftState: Equatable {
    enum Mode: Equatable {
        case off
        case once
        case locked
    }

    /// 两次点击间隔在这个时间内算双击
    static let doubleTapInterval: TimeInterval = 0.35

    private(set) var mode: Mode = .off
    private var lastTap: TimeInterval?

    var isUppercase: Bool { mode != .off }

    /// 点了一下上档键。time 是单调时间（秒）
    mutating func tap(at time: TimeInterval) {
        let isDoubleTap = lastTap.map { time - $0 <= Self.doubleTapInterval } ?? false
        switch mode {
        case .off:
            mode = .once
        case .once:
            mode = isDoubleTap ? .locked : .off
        case .locked:
            mode = .off
        }
        // 锁定之后的那一下不再参与双击判断
        lastTap = mode == .once ? time : nil
    }

    /// 打完一个字母
    mutating func didTypeLetter() {
        if mode == .once { mode = .off }
        lastTap = nil
    }

    mutating func reset() {
        mode = .off
        lastTap = nil
    }
}

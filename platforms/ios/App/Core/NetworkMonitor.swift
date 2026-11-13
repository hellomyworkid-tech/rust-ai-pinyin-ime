import Foundation
import Network
import Combine

/// 当前网络是否按流量计费（蜂窝网络、个人热点），下载大文件前要提醒用户。
@MainActor
final class NetworkMonitor: ObservableObject, NetworkConditions {
    @Published private(set) var isExpensive = false

    private let monitor = NWPathMonitor()

    /// forceExpensive：调试用，固定显示成蜂窝网络，不去监听真实网络
    init(forceExpensive: Bool = false) {
        if forceExpensive {
            isExpensive = true
            return
        }
        monitor.pathUpdateHandler = { [weak self] path in
            let expensive = path.isExpensive
            Task { @MainActor in self?.isExpensive = expensive }
        }
        monitor.start(queue: DispatchQueue(label: "com.zengji.aipinyin.network-path"))
    }

    deinit { monitor.cancel() }
}

import Foundation

/// 假下载通道：只记录调用，事件由测试手动发。
final class FakeDownloadTransport: DownloadTransport {
    var onEvent: ((DownloadEvent) -> Void)?

    struct Start: Equatable {
        let id: String
        let url: URL
        let resumeData: Data?
    }
    private(set) var starts: [Start] = []
    private(set) var pauses: [String] = []
    private(set) var cancels: [String] = []
    private(set) var reconnectCount = 0

    func start(id: String, url: URL, resumeData: Data?) { starts.append(Start(id: id, url: url, resumeData: resumeData)) }
    func pause(id: String) { pauses.append(id) }
    func cancel(id: String) { cancels.append(id) }
    func reconnect() { reconnectCount += 1 }

    func emit(_ event: DownloadEvent) { onEvent?(event) }
}

@MainActor
final class FakeNetwork: NetworkConditions {
    var isExpensive = false
}

import Foundation

/// 选词模型：给出上文、拼音和候选，答出其中一个候选。
/// Apple 端侧模型（AppleChoiceModel）实现它；测试里换成假模型。
@MainActor
protocol ChoiceModel: AnyObject {
    /// 模型此刻能不能用（设备不支持、Apple 智能没开、模型没下载完都算不能用）
    var isAvailable: Bool { get }
    /// 非阻塞读取已知的可用性缓存；返回 nil 表示尚未在后台探测完成（避免冷启动在主线程同步阻塞等待系统守护进程唤醒）
    var knownAvailability: Bool? { get }
    /// 在后台刷新并返回最新可用性（默认实现直接读 `isAvailable`，假模型测试零延迟完成）
    func refreshAvailability() async -> Bool
    /// 预热：提前把模型载入，首个请求就不用等一秒
    func prewarm()
    /// 答案应当是 request.candidates 里的一项；答不出返回 nil 或抛错
    func choose(_ request: IMEChoiceRequest) async throws -> String?
}

extension ChoiceModel {
    var knownAvailability: Bool? { isAvailable }
    func refreshAvailability() async -> Bool { isAvailable }
}

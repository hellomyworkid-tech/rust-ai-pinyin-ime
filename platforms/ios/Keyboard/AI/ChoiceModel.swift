import Foundation

/// 选词模型：给出上文、拼音和候选，答出其中一个候选。
/// Apple 端侧模型（AppleChoiceModel）实现它；测试里换成假模型。
@MainActor
protocol ChoiceModel: AnyObject {
    /// 模型此刻能不能用（设备不支持、Apple 智能没开、模型没下载完都算不能用）
    var isAvailable: Bool { get }
    /// 预热：提前把模型载入，首个请求就不用等一秒
    func prewarm()
    /// 答案应当是 request.candidates 里的一项；答不出返回 nil 或抛错
    func choose(_ request: IMEChoiceRequest) async throws -> String?
}

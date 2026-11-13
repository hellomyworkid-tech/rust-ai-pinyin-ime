import Foundation

/// Rust 引擎经 C FFI 返回的 JSON 的解码。格式以 Tests/Fixtures/*.json 为准：
///
///     {"action": "update", "view": { … }}                  按键被引擎吃掉，按新视图刷新
///     {"action": "commit", "text": "你好", "ghost": null}   上屏 text，之后回到空闲
///     {"action": "pass"}                                    引擎不处理，按键交还给键盘
///
/// 视图里界面用不到的字段（final_score、segment_spans 等）直接忽略。
enum IMEEngineJSON {
    enum DecodingFailure: Error, Equatable {
        case unknownAction(String)
        case missingField(String)
    }

    private struct Envelope: Decodable {
        let action: String
        let view: IMEView?
        let text: String?
        let ghost: String?
    }

    /// 每次按键都要解码一次，解码器只建一个
    private static let decoder: JSONDecoder = {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return decoder
    }()

    /// 解码一次按键的结果
    static func decodeResult(_ data: Data) throws -> IMEResult {
        let envelope = try decoder.decode(Envelope.self, from: data)
        switch envelope.action {
        case "update":
            guard let view = envelope.view else { throw DecodingFailure.missingField("view") }
            return .update(view)
        case "commit":
            guard let text = envelope.text else { throw DecodingFailure.missingField("text") }
            // 上屏之后引擎通常已经空闲；如果它带回了视图（输入还在继续）就用它的
            var view = envelope.view ?? .idle
            if view.ghostTextPreview == nil { view.ghostTextPreview = envelope.ghost }
            return .commit(text: text, view: view)
        case "pass":
            return .passThrough
        default:
            throw DecodingFailure.unknownAction(envelope.action)
        }
    }

    /// 只解码视图（currentView、refreshAfterAI 用）
    static func decodeView(_ data: Data) throws -> IMEView {
        try decoder.decode(IMEView.self, from: data)
    }

    /// ime_engine_candidates 的结果：{"total":86,"offset":30,"candidates":[{…},…]}
    struct CandidateRange: Decodable, Equatable {
        let total: Int
        let offset: Int
        let candidates: [IMECandidate]
    }

    static func decodeCandidateRange(_ data: Data) throws -> CandidateRange {
        try decoder.decode(CandidateRange.self, from: data)
    }

    /// 引擎发给宿主的选词请求：{"id":7,"context":"…","pinyin":"shiyan","candidates":["实验","誓言"]}
    static func decodeChoiceRequest(_ data: Data) throws -> IMEChoiceRequest {
        try decoder.decode(IMEChoiceRequest.self, from: data)
    }
}

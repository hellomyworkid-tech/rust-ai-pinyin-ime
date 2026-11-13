import Foundation

// 键盘界面与输入法引擎之间的唯一边界。
//
// 真正的实现是 RustEngine（通过 C FFI 调用 ime-session），MockEngine 只用于界面开发和测试。
// 界面层不得绕过这个协议直接依赖任何一种实现。
//
// 线程约定：协议里的方法全部在主线程调用。引擎内部的异步 AI 结果通过 onAsyncUpdate 通知，
// 实现方保证回调发生在主线程。

/// 送给引擎的按键
enum IMEKey: Equatable {
    /// 一个字符。中文模式下引擎只收字母（大写字母在空闲时放行）和输入中途的分词符 '；
    /// 数字、标点和其他字符一律返回 .passThrough——输入中途要先调用 commitFullSentence，
    /// 再由键盘自己插入（中文）标点，见 InputController.typeSymbol
    case character(Character)
    case space
    case enter
    case backspace
    /// 切换中 / 英文
    case toggleChineseEnglish
    case escape
    /// 候选翻页：只移动 IMEView.visibleCandidates 所在的那一页。
    /// 触屏键盘不用它们（「加载更多」用 candidates(from:count:)，引擎始终停在第 0 页）
    case pageUp
    case pageDown
    /// 拼音光标按音节左右移动
    case cursorLeft
    case cursorRight
}

/// 引擎处理一次输入后的结果
enum IMEResult {
    /// 引擎吃掉了按键，界面按新视图刷新
    case update(IMEView)
    /// 把 text 写进宿主输入框，然后按新视图刷新（通常已回到空闲状态）
    case commit(text: String, view: IMEView)
    /// 引擎不处理，由键盘自己把按键交给宿主（例如空闲时的空格、回车、退格）
    case passThrough
}

/// 一段输入区间：音节或分词块，对应 Rust 侧的 SegmentSpan
struct IMESpan: Decodable, Equatable {
    /// 在原始输入串里的字节区间 [start, end)
    let start: Int
    let end: Int
    /// 这一段当前对应的文字（音节栏里显示 codeKey，整句预览里显示 text）
    let text: String
    /// 这一段的拼音 / 英文编码
    let codeKey: String
}

/// 一个候选，对应 Rust 侧的 Candidate（只解码界面用得到的字段）
struct IMECandidate: Decodable, Equatable {
    let text: String
    let preedit: String
    /// 覆盖了多少字节的输入；等于输入总长表示整句候选
    let consumedLen: Int
    /// 说明标签，如 "[整句]"、"[简拼]"、"[AI 优选]"
    let tag: String
    /// 是否由 AI 提到首位
    let aiBoosted: Bool
}

/// 整句预览里的一段：(文字, 是否已被用户选定锁定)。Rust 侧是元组，JSON 里是 ["文字", true]
struct IMESentencePart: Decodable, Equatable {
    let text: String
    let pinned: Bool

    init(text: String, pinned: Bool) {
        self.text = text
        self.pinned = pinned
    }

    init(from decoder: Decoder) throws {
        var container = try decoder.unkeyedContainer()
        text = try container.decode(String.self)
        pinned = try container.decode(Bool.self)
    }
}

/// 界面渲染所需的全部状态，对应 Rust 侧 CandidateWindowViewModel 的 JSON。
/// 用 JSONDecoder.keyDecodingStrategy = .convertFromSnakeCase 解码。
struct IMEView: Decodable, Equatable {
    /// 是否正在输入（有拼音串）
    var visible = false
    /// 预编辑串（已按音节加分隔）
    var preeditText = ""
    var pageIndex = 0
    var pageSize = 0
    var totalCandidates = 0
    /// 当前页的候选
    var visibleCandidates: [IMECandidate] = []
    /// 当前页内的高亮下标
    var selectedCandidateIndex = 0
    /// 实时整句预览
    var composedFullSentence = ""
    var fullSentenceParts: [IMESentencePart] = []
    /// 首选整句拆出的逐个音节（预编辑栏按它画，可点击）
    var syllableSpans: [IMESpan] = []
    /// 拼音光标的字节位置
    var cursorBytePos = 0
    /// 光标所在音节；非 nil 时候选是「这个音节的选词」
    var activeSyllableIndex: Int?
    /// AI 语境提示（调试 / 状态展示用）
    var aiSemanticHint = ""
    /// 续写建议（目前引擎侧未启用）
    var ghostTextPreview: String?

    static let idle = IMEView()
}

/// 引擎请宿主帮忙选词（Apple 端侧模型或云端），宿主算完后调用 deliverChoice
struct IMEChoiceRequest: Decodable, Equatable {
    let id: UInt64
    /// 光标前的上文（引擎已截到合适长度）
    let context: String
    let pinyin: String
    /// 候选文本，答案必须是其中之一
    let candidates: [String]
}

protocol IMEEngine: AnyObject {
    func handle(_ key: IMEKey) -> IMEResult
    /// 点选候选。index 是全部候选里的下标（pageIndex * pageSize + 页内下标，与 candidates(from:count:) 的下标一致）
    func selectCandidate(at globalIndex: Int) -> IMEResult
    /// 取全部候选里的 [offset, offset + count)，不改变引擎状态（不翻页）。候选栏「加载更多」用它，
    /// 引擎始终停在第 0 页，空格上屏的就是用户看到的第一个候选。超出范围的部分不返回，空闲时返回空数组
    func candidates(from offset: Int, count: Int) -> [IMECandidate]
    /// 点击预编辑栏里的第 index 个音节，把拼音光标移过去
    func tapSyllable(at index: Int) -> IMEResult
    /// 把当前整句预览直接上屏。
    /// 引擎不处理标点：输入中途按标点时，由键盘先调用它上屏整句，再自己插入对应的中文标点
    /// （见 Tests/Fixtures/commit_with_punctuation.json：把 "," 当字符送进引擎只会并入拼音串）。
    func commitFullSentence() -> IMEResult
    func currentView() -> IMEView
    /// AI 结果到达后重排候选并返回新视图
    func refreshAfterAI() -> IMEView
    /// 宿主输入框光标前的文本，作为整句解码和 AI 选词的上文。
    /// nil 的意思是「沿用引擎自己记下的、之前输入框里的上屏历史」，不是「没有上文」：
    /// 一段输入开始时键盘刚问过宿主，宿主给 nil（空输入框通常如此）要传 ""
    func setContext(_ textBeforeCursor: String?)
    /// 取消当前输入
    func reset()
    var isComposing: Bool { get }
    /// 当前是否处于英文直通模式
    var isEnglishMode: Bool { get }

    /// AI 异步结果就绪（主线程回调）。收到后调用 refreshAfterAI 并刷新界面
    var onAsyncUpdate: (() -> Void)? { get set }
    /// 引擎请求宿主选词（主线程回调）。没有设置时引擎不会发请求
    var onChoiceRequest: ((IMEChoiceRequest) -> Void)? { get set }
    /// 引擎作废了编号为 id 的选词请求（输入结束、被新请求取代、换了后端；主线程回调）。
    /// 收到后可以中止对应的计算，不必也不应再 deliverChoice。可能先于同一编号的请求到达
    var onChoiceCancel: ((UInt64) -> Void)? { get set }
    /// 交回选词结果；text 为 nil 表示放弃（超时、模型不可用、被新输入取代）
    func deliverChoice(id: UInt64, text: String?)

    /// 应用设置（模糊音等）。键盘每次出现时调用一次
    func apply(_ settings: IMESettings)
    /// 个人词库文件被别的键盘进程改过就重新读入（返回 true）；没变、没启用持久化或读取失败返回 false。
    /// 键盘每次出现时、用户能上屏之前调用：否则这个引擎下次写盘会盖掉别的进程学到的词。
    /// 在调用线程上做文件 I/O（通常只是一次 stat）
    @discardableResult
    func reloadUserDictionaryIfChanged() -> Bool
    /// 加载 / 卸载进程内的本地模型。加载可能耗时数百毫秒，调用方放到后台队列
    func loadLocalModel(at url: URL) throws
    func unloadLocalModel()
    var isLocalModelLoaded: Bool { get }
}

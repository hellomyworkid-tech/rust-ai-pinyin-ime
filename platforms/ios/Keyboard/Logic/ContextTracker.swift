import Foundation

/// 本地维护的「光标前上文」，作为引擎整句解码和 AI 选词的上文。
///
/// 不在每次按键时去问 textDocumentProxy：只在一段输入开始、而本地副本不可信时读一次，
/// 之后键盘自己上屏、删除时同步更新。宿主那边文本或光标变了（textDidChange）就标记为不可信。
struct ContextTracker {
    /// 传给引擎的上文最多这么多字
    static let maxLength = 64

    /// 光标前最多 maxLength 个字符；nil 表示宿主没给
    private(set) var text: String?
    /// 本地副本是否还和宿主一致
    private(set) var isFresh = false

    /// 用宿主给的文本重建本地副本
    mutating func refresh(from proxyText: String?) {
        text = proxyText.map(Self.tail)
        isFresh = true
    }

    mutating func invalidate() {
        isFresh = false
    }

    /// 键盘自己上屏了一段文字
    mutating func didInsert(_ inserted: String) {
        guard isFresh, !inserted.isEmpty else { return }
        text = Self.tail((text ?? "") + inserted)
    }

    /// 键盘自己删了光标前一个字符
    mutating func didDeleteBackward() {
        guard isFresh, var current = text, !current.isEmpty else { return }
        // 副本已经是截断过的满长度：删掉一个以后，最前面本该露出来的字我们不知道，下次重新读
        if current.count >= Self.maxLength {
            isFresh = false
            return
        }
        current.removeLast()
        text = current
    }

    static func tail(_ s: String) -> String {
        s.count <= maxLength ? s : String(s.suffix(maxLength))
    }
}

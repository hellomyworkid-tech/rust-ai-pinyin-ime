import Foundation

/// 记录型的宿主输入框：记下每次写入 / 删除，并模拟宿主里的文本
final class RecordingTextOutput: TextOutput {
    enum Operation: Equatable {
        case insert(String)
        case delete
    }

    private(set) var operations: [Operation] = []
    /// 宿主输入框里光标前的全部文本
    var hostText = ""
    /// 宿主只给这么长的上文（真机大约两三百字）
    var contextWindow = 250
    /// 模拟宿主拿不到上文的情况
    var contextUnavailable = false
    private(set) var contextReads = 0

    var documentContextBeforeInput: String? {
        contextReads += 1
        return contextUnavailable ? nil : String(hostText.suffix(contextWindow))
    }

    func insertText(_ text: String) {
        operations.append(.insert(text))
        hostText += text
    }

    func deleteBackward() {
        operations.append(.delete)
        if !hostText.isEmpty { hostText.removeLast() }
    }

    var inserted: [String] {
        operations.compactMap {
            if case .insert(let s) = $0 { return s }
            return nil
        }
    }
}

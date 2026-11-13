import Foundation

/// 键盘往宿主输入框写字的最小接口。
/// 键盘里由 UITextDocumentProxy 包一层实现（见 ProxyTextOutput），测试里用记录型假实现。
protocol TextOutput: AnyObject {
    /// 光标前的文本。系统只给一小段（两三百字），也可能是 nil
    var documentContextBeforeInput: String? { get }
    func insertText(_ text: String)
    func deleteBackward()
}

import UIKit

/// 把 TextOutput 接到 UIInputViewController 的 textDocumentProxy 上
final class ProxyTextOutput: TextOutput {
    private weak var controller: UIInputViewController?

    init(controller: UIInputViewController) {
        self.controller = controller
    }

    var documentContextBeforeInput: String? {
        MainActor.assumeIsolated { controller?.textDocumentProxy.documentContextBeforeInput }
    }

    func insertText(_ text: String) {
        MainActor.assumeIsolated { controller?.textDocumentProxy.insertText(text) }
    }

    func deleteBackward() {
        MainActor.assumeIsolated { controller?.textDocumentProxy.deleteBackward() }
    }
}

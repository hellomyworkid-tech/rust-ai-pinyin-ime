import UIKit

/// 只为一件事：后台下载完成后系统唤起 App，要把系统给的 completionHandler 交给下载通道。
final class AppDelegate: NSObject, UIApplicationDelegate {
    func application(_ application: UIApplication,
                     handleEventsForBackgroundURLSession identifier: String,
                     completionHandler: @escaping () -> Void) {
        // 模型和词库各有一条后台会话
        guard let transport = BackgroundDownloadTransport.instance(for: identifier) else {
            completionHandler()
            return
        }
        transport.setBackgroundCompletionHandler(completionHandler)
    }
}

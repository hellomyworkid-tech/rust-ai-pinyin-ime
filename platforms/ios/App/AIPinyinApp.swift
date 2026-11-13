import SwiftUI

@main
struct AIPinyinApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var model = AppModel.live()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            Group {
                #if DEBUG
                if ProcessInfo.processInfo.arguments.contains("-hostTest") {
                    HostTestView()
                } else {
                    RootView()
                }
                #else
                RootView()
                #endif
            }
            .environmentObject(model.settings)
            .environmentObject(model.downloads)
            .environmentObject(model.lexicon)
            .environmentObject(model.apple)
            .environmentObject(model.keyboard)
            .environmentObject(model.network)
            // 第一次打开时下载完整词库，失败了下次打开、回到前台时再试
            .task { model.ensureLexicon() }
            .onChange(of: scenePhase) { _, phase in
                guard phase == .active else { return }
                model.lexicon.refresh()
                model.ensureLexicon()
            }
        }
    }
}

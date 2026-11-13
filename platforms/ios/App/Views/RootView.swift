import SwiftUI

struct RootView: View {
    @EnvironmentObject private var settings: SettingsStore
    @EnvironmentObject private var downloads: ModelDownloadManager
    @EnvironmentObject private var apple: AppleModelMonitor
    @EnvironmentObject private var keyboard: KeyboardInstallMonitor
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        TabView {
            Tab("首页", systemImage: "keyboard") {
                NavigationStack { HomeView() }
            }
            Tab("AI 选词", systemImage: "sparkles") {
                NavigationStack { AIChoiceView() }
            }
            Tab("输入设置", systemImage: "slider.horizontal.3") {
                NavigationStack { InputSettingsView() }
            }
            Tab("关于", systemImage: "info.circle") {
                NavigationStack { AboutView() }
            }
        }
        .onChange(of: scenePhase) { _, phase in
            guard phase == .active else { return }
            // 从系统设置、Apple 智能设置回来：重新检查各种状态
            keyboard.refresh()
            apple.refresh()
            downloads.refresh()
        }
        .alert("设置没有保存", isPresented: Binding(
            get: { settings.saveError != nil },
            set: { if !$0 { settings.saveError = nil } })
        ) {
            Button("好") { settings.saveError = nil }
        } message: {
            Text(settings.saveError ?? "")
        }
    }
}

extension SettingsStore {
    /// 给开关、选择器用的绑定：改一下就存一次
    func binding<Value>(_ keyPath: WritableKeyPath<IMESettings, Value>) -> Binding<Value> {
        Binding(
            get: { self.settings[keyPath: keyPath] },
            set: { newValue in self.update { $0[keyPath: keyPath] = newValue } })
    }
}

import SwiftUI

struct RootView: View {
    enum SectionTab: Hashable {
        case home, ai, input, about
    }

    @EnvironmentObject private var settings: SettingsStore
    @EnvironmentObject private var downloads: ModelDownloadManager
    @EnvironmentObject private var apple: AppleModelMonitor
    @EnvironmentObject private var keyboard: KeyboardInstallMonitor
    @Environment(\.scenePhase) private var scenePhase
    @State private var selectedTab: SectionTab = .home

    var body: some View {
        TabView(selection: $selectedTab) {
            Tab("首页", systemImage: "keyboard", value: .home) {
                NavigationStack { HomeView() }
            }
            Tab("AI 选词", systemImage: "sparkles", value: .ai) {
                NavigationStack { AIChoiceView() }
            }
            Tab("输入设置", systemImage: "slider.horizontal.3", value: .input) {
                NavigationStack { InputSettingsView() }
            }
            Tab("关于", systemImage: "info.circle", value: .about) {
                NavigationStack { AboutView() }
            }
        }
        .onOpenURL { url in
            guard url.scheme?.lowercased() == "aipinyin" else { return }
            switch url.host?.lowercased() {
            case "ai":
                selectedTab = .ai
            case "settings", "input":
                selectedTab = .input
            case "about":
                selectedTab = .about
            default:
                selectedTab = .home
            }
        }
        .onChange(of: scenePhase) { _, phase in
            guard phase == .active else { return }
            // 从系统设置、Apple 智能设置或键盘快捷面板回来：重新检查各种状态
            settings.reload()
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

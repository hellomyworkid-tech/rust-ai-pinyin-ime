import SwiftUI

/// 输入设置：模糊音、按键音、触感。
struct InputSettingsView: View {
    @EnvironmentObject private var settings: SettingsStore

    var body: some View {
        Form {
            Section {
                fuzzyToggle("z / zh", example: "打 zi 也能出「知」（zhi）", \.fuzzy.zZh)
                fuzzyToggle("c / ch", example: "打 ci 也能出「吃」（chi）", \.fuzzy.cCh)
                fuzzyToggle("s / sh", example: "打 si 也能出「是」（shi）", \.fuzzy.sSh)
            } header: {
                Text("平舌 / 翘舌")
            } footer: {
                Text("打开后，两边的读音互相算数，反过来也一样。")
            }

            Section("前后鼻音") {
                fuzzyToggle("in / ing", example: "打 xin 也能出「星」（xing）", \.fuzzy.inIng)
                fuzzyToggle("en / eng", example: "打 fen 也能出「风」（feng）", \.fuzzy.enEng)
                fuzzyToggle("an / ang", example: "打 fan 也能出「房」（fang）", \.fuzzy.anAng)
            }

            Section("方言") {
                fuzzyToggle("n / l", example: "打 lan 也能出「难」（nan）", \.fuzzy.nL)
                fuzzyToggle("f / h", example: "打 hu 也能出「福」（fu）", \.fuzzy.fH)
            }

            Section("拼写纠错") {
                fuzzyToggle("韵母字母打反", example: "打 xign 也当作 xing", \.fuzzy.typoCorrection)
            }

            Section {
                Toggle(isOn: settings.binding(\.emojiCandidates)) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("候选里显示表情")
                        Text("打 mao，「猫」后面跟着 🐈 🐱").font(.footnote).foregroundStyle(.secondary)
                    }
                }
                .accessibilityIdentifier("toggle-emojiCandidates")
            } footer: {
                Text("表情来自 Unicode CLDR 的中文表情注释，中文词和 ok、lol 这类英文词后面都有。选中表情只上屏表情，不记进个人词库。")
            }

            Section {
                Toggle("按键音", isOn: settings.binding(\.keyClickSound))
                    .accessibilityIdentifier("toggle-keyClickSound")
                VStack(alignment: .leading, spacing: 4) {
                    Toggle("触感反馈", isOn: settings.binding(\.haptics))
                        .accessibilityIdentifier("toggle-haptics")
                    Text("需要打开「允许完全访问」才有效：设置 → 通用 → 键盘 → 键盘 → AI 拼音。没打开时这个开关不起作用。")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                }
            } header: {
                Text("按键")
            } footer: {
                Text("改动立即保存，键盘下次出现时生效。按键音还要看手机的静音开关和「系统声音与触感」设置。")
            }
        }
        .navigationTitle("输入设置")
    }

    private func fuzzyToggle(_ title: String, example: String, _ keyPath: WritableKeyPath<IMESettings, Bool>) -> some View {
        Toggle(isOn: settings.binding(keyPath)) {
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                Text(example)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            }
        }
        .accessibilityIdentifier("toggle-\(title)")
    }
}

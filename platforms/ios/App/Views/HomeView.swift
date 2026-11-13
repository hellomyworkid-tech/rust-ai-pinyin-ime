import SwiftUI
import UIKit

/// 首页：键盘有没有添加、怎么添加、完全访问是什么、试打字。
struct HomeView: View {
    @EnvironmentObject private var keyboard: KeyboardInstallMonitor
    @State private var sample = ""
    @FocusState private var sampleFocused: Bool

    var body: some View {
        Form {
            Section {
                statusRow
            } footer: {
                Text("检测方式：读取系统「已添加键盘」的列表。iOS 只告诉 App 键盘有没有被添加，看不出你此刻用的是哪个键盘。")
            }

            LexiconSection()

            Section("三步添加键盘") {
                step(1, "点下面的按钮，打开 AI 拼音的设置页，点进「键盘」。")
                step(2, "打开「AI 拼音」右边的开关。")
                step(3, "回到任意输入框，长按键盘左下角的地球键，选「AI 拼音」。")
                Button {
                    openSettings()
                } label: {
                    Label("打开设置", systemImage: "gearshape")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(.borderedProminent)
                .accessibilityIdentifier("openSettingsButton")
            }

            Section {
                ZStack(alignment: .topLeading) {
                    TextEditor(text: $sample)
                        .focused($sampleFocused)
                        .frame(minHeight: 120)
                        .scrollContentBackground(.hidden)
                        .accessibilityIdentifier("tryTextField")
                    if sample.isEmpty {
                        Text("点这里，试试 AI 拼音。打 nihao，点「你好」。")
                            .foregroundStyle(.tertiary)
                            .padding(.top, 8)
                            .padding(.leading, 5)
                            .allowsHitTesting(false)
                    }
                }
                if !sample.isEmpty {
                    Button("清空", role: .destructive) { sample = "" }
                }
            } header: {
                Text("试一试")
            } footer: {
                Text("键盘没有出现 AI 拼音时，长按地球键切换。")
            }
            Section {
                Text("AI 拼音的全部功能（包括 AI 选词）都在本机运行，无需开启「允许完全访问」。")
                Text("开启后仅增加按键触感反馈。键盘不联网，无论是否开启，输入内容都只在本机处理。")
            } header: {
                Text("完全访问")
            } footer: {
                Text("设置路径：设置 › 通用 › 键盘 › 键盘 › AI 拼音 › 允许完全访问")
            }

        }
        .navigationTitle("AI 拼音")
        .scrollDismissesKeyboard(.interactively)
    }

    private var statusRow: some View {
        HStack(spacing: 12) {
            Image(systemName: keyboard.isAdded ? "checkmark.circle.fill" : "exclamationmark.circle.fill")
                .font(.title2)
                .foregroundStyle(keyboard.isAdded ? Color.green : Color.orange)
            VStack(alignment: .leading, spacing: 2) {
                Text(keyboard.isAdded ? "键盘已添加" : "键盘还没有添加")
                    .font(.headline)
                Text(keyboard.isAdded ? "在任意输入框里切到 AI 拼音就能用。" : "按下面的三步添加。")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("keyboardStatus")
    }

    private func step(_ number: Int, _ text: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Text("\(number)")
                .font(.subheadline.weight(.semibold))
                .foregroundStyle(.white)
                .frame(width: 24, height: 24)
                .background(Circle().fill(Color.accentColor))
            Text(text)
        }
    }

    private func openSettings() {
        guard let url = URL(string: UIApplication.openSettingsURLString) else { return }
        UIApplication.shared.open(url)
    }
}

/// 首页的「词库」一栏：完整词库下载到哪一步了。
/// 完整词库来自雾凇拼音（GPL-3.0），不在安装包里，App 第一次打开时自动下载（见 AppModel.ensureLexicon）
private struct LexiconSection: View {
    @EnvironmentObject private var lexicon: LexiconDownloadManager

    var body: some View {
        if let release = lexicon.catalog.first {
            Section {
                row(release)
                    .accessibilityIdentifier("lexiconStatus")
            } header: {
                Text("词库")
            } footer: {
                Text("完整词库：约 57 万词条，基于雾凇拼音，首次启动时下载（\(ByteFormat.string(release.bytes))）。\n"
                     + "基础词库：约 6.5 万词条，基于 Android 拼音输入法词库，随 App 安装。\n"
                     + "完整词库就绪前使用基础词库。基础词库收词较少，缺少近年的新词和英文词。")
            }
        }
    }

    @ViewBuilder
    private func row(_ release: LexiconRelease) -> some View {
        switch lexicon.state(of: release) {
        case .downloaded:
            Label("完整词库已就绪", systemImage: "checkmark.circle.fill")
                .foregroundStyle(.green)
        case .downloading(let received, let total):
            progress("正在下载完整词库", received: received, total: total)
        case .paused(let received, let total):
            VStack(alignment: .leading, spacing: 6) {
                progress("完整词库下载已暂停", received: received, total: total)
                Button("继续下载") { lexicon.start(release) }
            }
        case .finishing:
            HStack(spacing: 10) {
                ProgressView()
                Text("正在校验、解压词库")
            }
        case .failed(let failure):
            VStack(alignment: .leading, spacing: 6) {
                Label("完整词库下载失败", systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
                Text(failure.message)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                Button("重试") { lexicon.start(release) }
                    .accessibilityIdentifier("lexiconRetry")
            }
        case .notDownloaded:
            VStack(alignment: .leading, spacing: 6) {
                #if DEBUG
                if AppModel.keyboardBundlesFullLexicon {
                    Text("调试构建：键盘自带完整词库，不自动下载")
                }
                #endif
                Text("完整词库还没有下载")
                Button("下载（约 \(ByteFormat.string(release.bytes))）") { lexicon.start(release) }
                    .accessibilityIdentifier("lexiconDownload")
            }
        }
    }

    private func progress(_ title: String, received: Int64, total: Int64) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title)
            ProgressView(value: Double(received), total: Double(max(total, max(received, 1))))
            Text("\(ByteFormat.string(received)) / \(ByteFormat.string(total))")
                .font(.footnote.monospacedDigit())
                .foregroundStyle(.secondary)
        }
    }
}

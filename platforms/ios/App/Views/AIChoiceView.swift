import SwiftUI
import UIKit

/// AI 选词：选方式；Apple 端侧模型显示实时可用状态；本地模型管理下载。
struct AIChoiceView: View {
    @EnvironmentObject private var settings: SettingsStore
    @EnvironmentObject private var apple: AppleModelMonitor
    @EnvironmentObject private var downloads: ModelDownloadManager
    @EnvironmentObject private var network: NetworkMonitor

    var body: some View {
        Form {
            Section {
                ForEach(IMESettings.AIMode.allCases, id: \.self) { mode in
                    modeRow(mode)
                }
            } header: {
                Text("选词方式")
            } footer: {
                Text("AI 选词在你打字时，把最可能想打的词排到前面。改动立即保存，键盘下次出现时生效。")
            }

            switch settings.settings.aiMode {
            case .off:
                Section {
                    Text("键盘只按词频和规则排序候选，不调用任何模型。")
                        .font(.subheadline)
                }
            case .apple:
                AppleModelSection()
            case .local:
                LocalModelSection()
            }
        }
        .navigationTitle("AI 选词")
    }

    private func modeRow(_ mode: IMESettings.AIMode) -> some View {
        let selected = settings.settings.aiMode == mode
        return Button {
            settings.update { $0.aiMode = mode }
        } label: {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: selected ? "checkmark.circle.fill" : "circle")
                    .foregroundStyle(selected ? Color.accentColor : Color.secondary)
                    .font(.title3)
                VStack(alignment: .leading, spacing: 3) {
                    Text(Self.title(mode)).font(.headline).foregroundStyle(.primary)
                    Text(Self.summary(mode)).font(.footnote).foregroundStyle(.secondary)
                }
                Spacer(minLength: 0)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("mode-\(mode.rawValue)")
        .accessibilityAddTraits(selected ? .isSelected : [])
    }

    static func title(_ mode: IMESettings.AIMode) -> String {
        switch mode {
        case .off: return "关闭"
        case .apple: return "Apple 端侧模型"
        case .local: return "本地模型"
        }
    }

    static func summary(_ mode: IMESettings.AIMode) -> String {
        switch mode {
        case .off: return "只按规则选词，不使用任何模型。"
        case .apple: return "用系统自带的 Apple 智能模型。不用下载，几乎不占键盘内存；需要支持 Apple 智能的设备。实测选词不如本地模型准，每次约等半秒。"
        case .local: return "下载一个开源小模型，在键盘里运行。要下载约 400–500 MB，键盘运行时多占 50–80 MB 内存；实测选词更准，约 0.1 秒出结果。"
        }
    }
}

// MARK: - Apple 端侧模型

private struct AppleModelSection: View {
    @EnvironmentObject private var apple: AppleModelMonitor

    var body: some View {
        Section {
            HStack(spacing: 12) {
                Image(systemName: apple.status.isAvailable ? "checkmark.circle.fill" : "exclamationmark.triangle.fill")
                    .font(.title2)
                    .foregroundStyle(apple.status.isAvailable ? Color.green : Color.orange)
                Text(apple.status.title).font(.headline)
            }
            Text(apple.status.explanation).font(.subheadline)
            if apple.status == .appleIntelligenceNotEnabled {
                Button("打开设置") {
                    if let url = URL(string: UIApplication.openSettingsURLString) { UIApplication.shared.open(url) }
                }
            }
        } header: {
            Text("Apple 端侧模型状态")
        } footer: {
            Text("状态每隔几秒自动刷新。")
        }
        .accessibilityIdentifier("appleStatus")
        // 页面可见期间轮询：用户去系统设置里打开 Apple 智能、模型下载好，都会自己变
        .task {
            while !Task.isCancelled {
                apple.refresh()
                try? await Task.sleep(for: .seconds(3))
            }
        }
    }
}

// MARK: - 本地模型

private struct LocalModelSection: View {
    @EnvironmentObject private var settings: SettingsStore
    @EnvironmentObject private var downloads: ModelDownloadManager
    @EnvironmentObject private var network: NetworkMonitor

    var body: some View {
        Section {
            if network.isExpensive {
                Label("现在用的是蜂窝网络或个人热点，下载模型会消耗流量。", systemImage: "antenna.radiowaves.left.and.right")
                    .font(.subheadline)
                    .foregroundStyle(.orange)
            }
            if !hasUsableModel {
                Label("还没有可用的模型。下载并选中一个之前，键盘按规则选词。", systemImage: "info.circle")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
            ForEach(downloads.catalog) { model in
                ModelRow(model: model)
            }
        } header: {
            Text("可下载的模型")
        } footer: {
            Text("模型存在 App 和键盘共用的位置，不会进备份。iOS 对键盘的内存限制很严，剩余内存不够时键盘会暂时不加载模型，改按规则选词，不影响打字。")
        }
    }

    private var hasUsableModel: Bool {
        guard let id = settings.settings.localModelID else { return false }
        return downloads.downloadedIDs.contains(id)
    }
}

private struct ModelRow: View {
    let model: LocalModel
    @EnvironmentObject private var settings: SettingsStore
    @EnvironmentObject private var downloads: ModelDownloadManager

    /// 同一个视图上叠多个 confirmationDialog 只有最后一个会弹出，所以合成一个，按内容切换
    private enum Prompt { case cellular, delete }
    @State private var prompt: Prompt?
    @State private var spaceAlert: String?

    private var state: ModelDownloadState { downloads.state(of: model) }
    private var selected: Bool { settings.settings.localModelID == model.id }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            header
            Text("大小 \(ByteFormat.string(model.bytes)) · 键盘运行时约占 \(model.residentMiB) MB 内存")
                .font(.footnote)
                .foregroundStyle(.secondary)
            statusView
        }
        .padding(.vertical, 4)
        .confirmationDialog(
            prompt == .delete ? "删除「\(model.title)」？" : "现在用的是蜂窝网络或个人热点",
            isPresented: Binding(get: { prompt != nil }, set: { if !$0 { prompt = nil } }),
            titleVisibility: .visible
        ) {
            if prompt == .delete {
                Button("删除", role: .destructive) { downloads.delete(model) }
            } else {
                Button("继续下载（\(ByteFormat.string(model.bytes))）") { downloads.start(model) }
            }
            Button("取消", role: .cancel) {}
        } message: {
            if prompt == .delete {
                Text("文件会从这台设备上删除，以后想用要重新下载。")
            } else {
                Text("下载「\(model.title)」大约要用 \(ByteFormat.string(model.bytes)) 流量。建议连上 Wi-Fi 再下。")
            }
        }
        .alert("存储空间不够", isPresented: Binding(get: { spaceAlert != nil }, set: { if !$0 { spaceAlert = nil } })) {
            Button("好") { spaceAlert = nil }
        } message: {
            Text(spaceAlert ?? "")
        }
    }

    private var header: some View {
        HStack(alignment: .firstTextBaseline) {
            VStack(alignment: .leading, spacing: 2) {
                Text(model.title).font(.headline)
                Text(model.note).font(.footnote).foregroundStyle(.secondary)
            }
            Spacer()
            if state == .downloaded {
                Button {
                    settings.update { $0.localModelID = model.id }
                } label: {
                    Label(selected ? "使用中" : "使用", systemImage: selected ? "checkmark.circle.fill" : "circle")
                        .labelStyle(.titleAndIcon)
                }
                .buttonStyle(.borderless)
                .disabled(selected)
                .accessibilityIdentifier("use-\(model.id)")
            }
        }
    }

    @ViewBuilder
    private var statusView: some View {
        switch state {
        case .notDownloaded:
            button("下载", systemImage: "arrow.down.circle", id: "download") { begin() }

        case .downloading(let received, let total):
            progress(received: received, total: total, caption: nil)
            HStack(spacing: 16) {
                button("暂停", systemImage: "pause.circle", id: "pause") { downloads.pause(model) }
                button("取消", systemImage: "xmark.circle", role: .destructive, id: "cancel") { downloads.cancel(model) }
            }

        case .paused(let received, let total):
            progress(received: received, total: total, caption: "已暂停")
            HStack(spacing: 16) {
                button("继续", systemImage: "play.circle", id: "resume") { begin() }
                button("取消", systemImage: "xmark.circle", role: .destructive, id: "cancel") { downloads.cancel(model) }
            }

        case .finishing:
            HStack(spacing: 8) {
                ProgressView()
                Text("正在校验文件…").font(.subheadline).foregroundStyle(.secondary)
            }

        case .downloaded:
            HStack {
                Label("已下载", systemImage: "checkmark.seal").font(.subheadline).foregroundStyle(.green)
                Spacer()
                button("删除", systemImage: "trash", role: .destructive, id: "delete") { prompt = .delete }
            }

        case .failed(let failure):
            Label(failure.message, systemImage: "exclamationmark.triangle.fill")
                .font(.subheadline)
                .foregroundStyle(.red)
                .accessibilityIdentifier("failure-\(model.id)")
            HStack(spacing: 16) {
                button("重试", systemImage: "arrow.clockwise", id: "retry") { begin() }
                button("放弃", systemImage: "xmark.circle", role: .destructive, id: "cancel") { downloads.cancel(model) }
            }
        }
    }

    private func progress(received: Int64, total: Int64, caption: String?) -> some View {
        let fraction = total > 0 ? min(1, Double(received) / Double(total)) : 0
        let numbers = received > 0
            ? "\(ByteFormat.string(received)) / \(ByteFormat.string(total))（\(Int(fraction * 100))%）"
            : nil
        return VStack(alignment: .leading, spacing: 4) {
            ProgressView(value: fraction)
                .accessibilityIdentifier("progress-\(model.id)")
            Text([caption, numbers].compactMap { $0 }.joined(separator: " · "))
                .font(.footnote.monospacedDigit())
                .foregroundStyle(.secondary)
        }
    }

    private func button(_ title: String, systemImage: String, role: ButtonRole? = nil, id: String,
                        action: @escaping () -> Void) -> some View {
        Button(role: role, action: action) {
            Label(title, systemImage: systemImage)
        }
        .buttonStyle(.borderless)
        .accessibilityIdentifier("\(id)-\(model.id)")
    }

    /// 下载 / 继续 / 重试都先过检查：空间不够就说，蜂窝网络先问
    private func begin() {
        switch downloads.preflight(model) {
        case .ok:
            downloads.start(model)
        case .cellular:
            prompt = .cellular
        case .insufficientSpace(let needed, let available):
            spaceAlert = "下载「\(model.title)」需要约 \(ByteFormat.string(needed))（含余量），这台设备上只剩 \(ByteFormat.string(available))。清理一些空间后再试。"
        }
    }
}

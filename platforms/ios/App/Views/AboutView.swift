import SwiftUI

struct AboutView: View {
    private var version: String {
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? "?"
        let build = info?["CFBundleVersion"] as? String ?? "?"
        return "\(short)（\(build)）"
    }

    var body: some View {
        Form {
            Section {
                LabeledContent("版本", value: version)
            }

            Section {
                Text("你打的字只在这台设备上处理。键盘不使用网络，不收集、不上传任何输入内容；就算你打开了「允许完全访问」，也一样。")
                Text("AI 选词用的是 Apple 自带的端侧模型，或者你自己下载到手机里的本地模型，都在设备上运行。")
                Text("这个 App 只在下载文件时联网：第一次打开时从 GitHub 下载完整词库；你主动点「下载」时，从模型发布方的服务器下载本地模型。下载时不会发送你打的任何内容。")
            } header: {
                Text("隐私")
            }

            Section {
                credit("雾凇拼音 rime-ice", role: "完整词库的来源（第一次打开时下载，不在安装包里）", license: "GPL-3.0",
                       url: "https://github.com/iDvel/rime-ice")
                credit("Android 拼音输入法词库", role: "基础词库（完整词库下完之前用）", license: "Apache-2.0",
                       url: "https://android.googlesource.com/platform/packages/inputmethods/PinyinIME")
                credit("Unicode CLDR", role: "候选里的表情和它们的中文名", license: "Unicode-3.0",
                       url: "https://cldr.unicode.org")
                credit("llama.cpp", role: "本地模型的推理引擎", license: "MIT",
                       url: "https://github.com/ggml-org/llama.cpp")
                credit("通义千问 Qwen", role: "本地模型（Qwen2.5、Qwen3）", license: "Apache-2.0",
                       url: "https://huggingface.co/Qwen")
            } header: {
                Text("开源致谢")
            }

            Section("协议全文") {
                ForEach(LicenseText.bundled) { license in
                    NavigationLink(license.title) {
                        LicenseTextView(license: license)
                    }
                }
            }
        }
        .navigationTitle("关于")
    }

    private func credit(_ name: String, role: String, license: String, url: String) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text(name).font(.headline)
                Spacer()
                Text(license)
                    .font(.footnote.monospaced())
                    .foregroundStyle(.secondary)
            }
            Text(role).font(.subheadline)
            if let link = URL(string: url) {
                Link(url.replacingOccurrences(of: "https://", with: ""), destination: link)
                    .font(.footnote)
            }
        }
        .padding(.vertical, 2)
    }
}

/// 打进安装包的第三方内容的协议全文（App/Licenses 下的文本文件）
struct LicenseText: Identifiable {
    let title: String
    let resource: String
    var id: String { resource }

    static let bundled = [
        LicenseText(title: "Android 拼音输入法词库（Apache-2.0）", resource: "AOSP-PinyinIME-NOTICE"),
        LicenseText(title: "Unicode CLDR（Unicode License v3）", resource: "Unicode-CLDR-LICENSE"),
        LicenseText(title: "llama.cpp（MIT）", resource: "llama.cpp-LICENSE"),
    ]

    var text: String {
        guard let url = Bundle.main.url(forResource: resource, withExtension: "txt"),
              let text = try? String(contentsOf: url, encoding: .utf8) else { return "（没有找到协议文本）" }
        return text
    }
}

private struct LicenseTextView: View {
    let license: LicenseText

    var body: some View {
        ScrollView {
            Text(license.text)
                .font(.footnote.monospaced())
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding()
        }
        .navigationTitle(license.title)
        .navigationBarTitleDisplayMode(.inline)
    }
}

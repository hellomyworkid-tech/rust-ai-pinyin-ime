#if DEBUG
import SwiftUI

/// UI 测试用的宿主：几个类型不同的输入框（启动参数 -hostTest 才会出现）。
/// 环境变量 HOST_TEST_TEXT 给中文输入框预填一段文字，用来测「上文决定选词」。
struct HostTestView: View {
    @State private var text = ProcessInfo.processInfo.environment["HOST_TEST_TEXT"] ?? ""
    @State private var url = ""
    @State private var number = ""

    init() {
        HostTestSetup.apply()
    }

    var body: some View {
        NavigationStack {
            Form {
                Section("中文") {
                    TextField("在这里试打字", text: $text, axis: .vertical)
                        .lineLimit(3...6)
                        .accessibilityIdentifier("hostTextField")
                }
                Section("网址（键盘应从英文开始）") {
                    TextField("example.com", text: $url)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .accessibilityIdentifier("urlTextField")
                }
                Section("数字（键盘应打开数字页）") {
                    TextField("123", text: $number)
                        .keyboardType(.numberPad)
                        .accessibilityIdentifier("numberTextField")
                }
            }
            .navigationTitle("AI 拼音")
        }
    }
}

/// UI 测试的准备工作：每次以 -hostTest 启动都把设置写成已知状态，测试之间互不影响。
/// - HOST_TEST_AI_MODE=off|apple|local：AI 选词方式（默认是设置的默认值）；
/// - HOST_TEST_MODEL=<Mac 上的 GGUF 路径> 和 HOST_TEST_MODEL_ID=<ModelCatalog 里的 id>：
///   把模型放进 App Group 的模型目录并选中它（只在模拟器里有意义：模拟器看得到 Mac 的文件，
///   同一卷上的拷贝是瞬间完成的克隆）。
enum HostTestSetup {
    private static var applied = false

    /// 视图结构体会被 SwiftUI 重建多次，准备工作只做一次
    static func apply(environment: [String: String] = ProcessInfo.processInfo.environment) {
        guard !applied else { return }
        applied = true
        var settings = IMESettings()
        if let raw = environment["HOST_TEST_AI_MODE"], let mode = IMESettings.AIMode(rawValue: raw) {
            settings.aiMode = mode
        }
        if let path = environment["HOST_TEST_MODEL"], !path.isEmpty,
           let model = ModelCatalog.find(environment["HOST_TEST_MODEL_ID"]),
           let destination = model.localURL {
            let files = FileManager.default
            if !model.isDownloaded {
                try? files.createDirectory(at: destination.deletingLastPathComponent(), withIntermediateDirectories: true)
                try? files.removeItem(at: destination)
                try? files.copyItem(at: URL(fileURLWithPath: path), to: destination)
            }
            if model.isDownloaded {
                settings.aiMode = .local
                settings.localModelID = model.id
            }
        }
        try? settings.save()
    }
}
#endif

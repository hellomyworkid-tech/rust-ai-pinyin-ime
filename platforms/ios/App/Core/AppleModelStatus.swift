import Foundation
import FoundationModels

/// Apple 端侧模型（FoundationModels）此刻能不能用，以及给用户看的解释。
enum AppleModelStatus: Equatable {
    case available
    /// 这台设备不支持 Apple 智能
    case deviceNotEligible
    /// 设备支持，但用户没在系统设置里打开 Apple 智能
    case appleIntelligenceNotEnabled
    /// 模型还在下载 / 准备
    case modelNotReady
    case unknown

    init(_ availability: SystemLanguageModel.Availability) {
        switch availability {
        case .available:
            self = .available
        case .unavailable(let reason):
            switch reason {
            case .deviceNotEligible: self = .deviceNotEligible
            case .appleIntelligenceNotEnabled: self = .appleIntelligenceNotEnabled
            case .modelNotReady: self = .modelNotReady
            @unknown default: self = .unknown
            }
        @unknown default:
            self = .unknown
        }
    }

    static func current() -> AppleModelStatus {
        AppleModelStatus(SystemLanguageModel.default.availability)
    }

    var isAvailable: Bool { self == .available }

    var title: String {
        switch self {
        case .available: return "可用"
        case .deviceNotEligible: return "这台设备用不了"
        case .appleIntelligenceNotEnabled: return "Apple 智能还没打开"
        case .modelNotReady: return "模型还没准备好"
        case .unknown: return "暂时无法判断"
        }
    }

    var explanation: String {
        switch self {
        case .available:
            return "模型已经就绪，选词在这台 iPhone 上完成，不需要下载，也不联网。"
        case .deviceNotEligible:
            return "这台设备（或所在的国家和地区）用不了 Apple 智能，所以没有端侧模型。已经下载并选中了本地模型的话，键盘会自动改用它；否则按词频和规则选词。"
        case .appleIntelligenceNotEnabled:
            return "这台设备支持，但 Apple 智能还没有打开。到「设置」→「Apple 智能与 Siri」里打开，回到这里会自动更新。在此之前键盘改用已下载的本地模型；没有本地模型就按词频和规则选词。"
        case .modelNotReady:
            return "系统正在下载或准备模型，连着 Wi-Fi 和电源时通常几分钟就好。准备好之前键盘改用已下载的本地模型（没有就按词频和规则选词），这里会自动更新。"
        case .unknown:
            return "系统没有给出明确的状态。键盘每次出现时会再检查一次；不可用时改用已下载并选中的本地模型，没有就按词频和规则选词。"
        }
    }
}

@MainActor
final class AppleModelMonitor: ObservableObject {
    @Published private(set) var status: AppleModelStatus

    private let read: () -> AppleModelStatus

    init(read: @escaping () -> AppleModelStatus = AppleModelStatus.current) {
        self.read = read
        status = read()
    }

    func refresh() {
        let next = read()
        if next != status { status = next }
    }
}

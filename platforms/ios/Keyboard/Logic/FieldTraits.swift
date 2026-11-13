import UIKit

/// 宿主输入框的特征，决定键盘的起始模式和回车键文字
struct FieldTraits: Equatable {
    var keyboardType: UIKeyboardType = .default
    var returnKeyType: UIReturnKeyType = .default

    /// 网址、邮箱、纯 ASCII 和各种数字键盘一律从英文开始
    var startsInEnglish: Bool {
        switch keyboardType {
        case .URL, .emailAddress, .asciiCapable, .asciiCapableNumberPad,
             .numberPad, .phonePad, .decimalPad, .numbersAndPunctuation:
            return true
        default:
            return false
        }
    }

    /// 数字类输入框直接打开数字页
    var startPage: KeyboardPage {
        switch keyboardType {
        case .numberPad, .phonePad, .decimalPad, .numbersAndPunctuation, .asciiCapableNumberPad:
            return .numbers
        default:
            return .letters
        }
    }

    /// 回车键文字。输入中途回车是把拼音原样上屏，显示「确认」
    func returnKeyTitle(isComposing: Bool) -> String {
        if isComposing { return "确认" }
        switch returnKeyType {
        case .go: return "前往"
        case .google, .yahoo, .search: return "搜索"
        case .join: return "加入"
        case .next: return "下一项"
        case .route: return "路线"
        case .send: return "发送"
        case .done: return "完成"
        case .emergencyCall: return "紧急呼叫"
        case .continue: return "继续"
        default: return "换行"
        }
    }

    /// 非默认的回车键（发送、搜索等）用强调色，和系统键盘一致
    var returnKeyIsProminent: Bool {
        returnKeyType != .default
    }
}

import UIKit

/// 键盘配色，浅色 / 深色自动切换
enum KeyboardTheme {
    private static func dynamic(light: UIColor, dark: UIColor) -> UIColor {
        UIColor { $0.userInterfaceStyle == .dark ? dark : light }
    }

    private static func rgb(_ r: CGFloat, _ g: CGFloat, _ b: CGFloat, _ a: CGFloat = 1) -> UIColor {
        UIColor(red: r / 255, green: g / 255, blue: b / 255, alpha: a)
    }

    /// 与系统在键盘下方画的那一条（地球键、麦克风）同色，衔接处看不出分界
    static let background = dynamic(light: rgb(224, 225, 232), dark: rgb(32, 32, 32))
    static let letterKey = dynamic(light: .white, dark: rgb(108, 108, 112))
    static let functionKey = dynamic(light: rgb(171, 176, 186), dark: rgb(70, 70, 74))
    /// 字母键按下时的颜色（同功能键）；功能键按下时变成字母键的颜色
    static let letterKeyPressed = functionKey
    static let functionKeyPressed = letterKey
    static let keyShadow = dynamic(light: rgb(136, 138, 142), dark: rgb(20, 20, 20))
    static let keyText = dynamic(light: .black, dark: .white)
    static let secondaryText = dynamic(light: rgb(60, 60, 67, 0.7), dark: rgb(235, 235, 245, 0.65))
    static let accent = UIColor.systemBlue
    static let accentText = UIColor.white
    /// 已选定的字、AI 标记
    static let pinnedText = dynamic(light: rgb(0, 102, 214), dark: rgb(100, 170, 255))
    static let aiMarker = dynamic(light: rgb(175, 82, 222), dark: rgb(205, 130, 255))
    /// 首选候选的底色
    static let firstCandidateBackground = dynamic(light: rgb(255, 255, 255, 0.75), dark: rgb(255, 255, 255, 0.14))
    static let candidatePressed = dynamic(light: rgb(0, 0, 0, 0.12), dark: rgb(255, 255, 255, 0.2))
    static let separator = dynamic(light: rgb(0, 0, 0, 0.12), dark: rgb(255, 255, 255, 0.12))
    static let previewBackground = dynamic(light: .white, dark: rgb(120, 120, 124))
}

import Foundation

/// 中文模式下标点键的映射：键位按 ASCII 定义，中文模式输出全角中文标点，英文模式原样输出。
/// 数字和没有对应中文标点的符号（# % + = @ 等）两种模式都原样输出。
enum PunctuationMap {
    private static let chinese: [String: String] = [
        ",": "，", ".": "。", "?": "？", "!": "！", ":": "：", ";": "；",
        "(": "（", ")": "）", "[": "【", "]": "】", "{": "｛", "}": "｝",
        "<": "《", ">": "》", "\\": "、", "^": "……", "_": "——", "~": "～",
        "$": "￥", "¥": "￥", "`": "·", "•": "·",
    ]

    /// 键上显示的文字
    static func label(for ascii: String, chineseMode: Bool) -> String {
        guard chineseMode else { return ascii }
        switch ascii {
        case "\"": return "“”"
        case "'": return "‘’"
        default: return chinese[ascii] ?? ascii
        }
    }

    /// 按下后实际上屏的文字。引号要看上文决定是左引号还是右引号
    static func output(for ascii: String, chineseMode: Bool, contextBefore: String?) -> String {
        guard chineseMode else { return ascii }
        switch ascii {
        case "\"": return pairedQuote(open: "“", close: "”", contextBefore: contextBefore)
        case "'": return pairedQuote(open: "‘", close: "’", contextBefore: contextBefore)
        default: return chinese[ascii] ?? ascii
        }
    }

    /// 上文里最近的一个同类引号是左引号，就补右引号；否则开一个新的左引号
    private static func pairedQuote(open: Character, close: Character, contextBefore: String?) -> String {
        guard let context = contextBefore,
              let last = context.last(where: { $0 == open || $0 == close }) else {
            return String(open)
        }
        return String(last == open ? close : open)
    }
}

import CoreText
import Foundation

/// 候选文字能不能用系统字体显示出来。
///
/// 完整词库（雾凇拼音）收了不少 CJK 扩展 B 及以后的生僻字（如 `yu` 下的 U+2B6AA），iOS 自带字体覆盖不全，
/// 显示出来是一个带问号的方框。这类候选没法看清，也几乎不会被选，直接不显示。
///
/// 只检查生僻区（CJK 扩展 A、第 2/3 平面的扩展 B~H、私用区），常用汉字、字母、表情直接放行；
/// 每个码位只查一次字体（结果缓存），之后是一次字典查找。
enum CandidateRenderability {
    private static let lock = NSLock()
    private static var cache: [UInt32: Bool] = [:]
    private static let baseFont: CTFont = CTFontCreateUIFontForLanguage(.system, 17, "zh-Hans" as CFString)
        ?? CTFontCreateWithName("PingFangSC-Regular" as CFString, 17, nil)

    /// 文字里的每个字都有字形（不会显示成方框）
    static func isRenderable(_ text: String) -> Bool {
        for scalar in text.unicodeScalars where needsCheck(scalar) {
            if !isRenderable(scalar) { return false }
        }
        return true
    }

    /// 需要查字体的码位：CJK 扩展 A、第 2/3 平面（扩展 B~H）、私用区
    static func needsCheck(_ scalar: Unicode.Scalar) -> Bool {
        switch scalar.value {
        case 0x3400...0x4DBF, 0x20000...0x3FFFF, 0xE000...0xF8FF, 0xF0000...0x10FFFF: return true
        default: return false
        }
    }

    static func isRenderable(_ scalar: Unicode.Scalar) -> Bool {
        lock.lock()
        if let known = cache[scalar.value] {
            lock.unlock()
            return known
        }
        lock.unlock()
        let result = lookUpGlyph(scalar)
        lock.lock()
        cache[scalar.value] = result
        lock.unlock()
        return result
    }

    private static func lookUpGlyph(_ scalar: Unicode.Scalar) -> Bool {
        let string = String(scalar)
        let cfString = string as CFString
        // 系统字体加上系统的回退字体链：找不到任何字体时回退到 LastResort（只有方框字形）
        let font = CTFontCreateForString(baseFont, cfString, CFRange(location: 0, length: CFStringGetLength(cfString)))
        let name = CTFontCopyPostScriptName(font) as String
        if name.contains("LastResort") { return false }
        var units = Array(string.utf16)
        var glyphs = [CGGlyph](repeating: 0, count: units.count)
        guard CTFontGetGlyphsForCharacters(font, &units, &glyphs, units.count) else { return false }
        return glyphs.allSatisfy { $0 != 0 }
    }
}

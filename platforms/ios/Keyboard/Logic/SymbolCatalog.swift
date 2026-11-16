import CoreGraphics
import Foundation

/// 符号面板的分类
enum SymbolCategory: String, CaseIterable, Equatable {
    case common
    case chinese
    case english
    case network
    case math
    case numbers
    case units
    case arrows
    case pinyin
    case greek
    case special
    case kaomoji

    var title: String {
        switch self {
        case .common: return "常用"
        case .chinese: return "中文"
        case .english: return "英文"
        case .network: return "网络"
        case .math: return "数学"
        case .numbers: return "序号"
        case .units: return "单位"
        case .arrows: return "箭头"
        case .pinyin: return "拼音"
        case .greek: return "希腊"
        case .special: return "特殊"
        case .kaomoji: return "颜文字"
        }
    }
}

/// 符号面板里的一个符号项
struct SymbolItem: Equatable {
    let text: String
    /// 右上角是否显示「全」角标（用于与半角符号外形相近或属于中文全角标点的字符，类似元输入法）
    let showsFullWidthBadge: Bool

    init(_ text: String, fullWidth: Bool? = nil) {
        self.text = text
        self.showsFullWidthBadge = fullWidth ?? SymbolCatalog.isFullWidthSymbol(text)
    }
}

/// 符号面板网格里的单元格（含末尾补齐空白格）
struct SymbolGridSlot: Equatable {
    let item: SymbolItem?
    let span: Int
    let width: CGFloat
    /// 是否在这一行最右端（最右端不画竖向分隔线）
    let isRowEnd: Bool
}

enum SymbolCatalog {
    /// 需要标注「全」角标的全角标点与成对括号（让用户在常用/中文混排时一眼区分全角与半角）
    private static let fullWidthSymbols: Set<String> = [
        "，", "。", "、", "；", "：", "？", "！", "……", "——", "～",
        "“", "”", "‘", "’", "「", "」", "『", "』", "〝", "〞",
        "（", "）", "【", "】", "《", "》", "〈", "〉", "〔", "〕",
        "〖", "〗", "〘", "〙", "〚", "〛", "［", "］", "｛", "｝",
        "﹏", "〰", "‖", "｜", "／", "＼", "＋", "－", "＝", "＜",
        "＞", "＠", "＃", "￥", "％", "＆", "＊", "｀", "＾",
    ]

    static func isFullWidthSymbol(_ text: String) -> Bool {
        fullWidthSymbols.contains(text)
    }

    static func items(for category: SymbolCategory) -> [SymbolItem] {
        rawStrings(for: category).map { SymbolItem($0) }
    }

    private static func rawStrings(for category: SymbolCategory) -> [String] {
        switch category {
        case .common:
            return [
                // 首屏兼顾高频中文全角标点/成对括号与高频半角符号，且所有成对引号/括号同行成对排布
                "，", "。", "、", "：", "；",
                "？", "！", "……", "——", "·",
                "“", "”", "（", "）", "～",
                "【", "】", "《", "》", "@",
                "「", "」", "_", "-", "/",
                "『", "』", "+", "=", "*",
                ".", ",", ":", ";", "?",
                "!", "\"", "'", "\\", "|",
                "#", "$", "￥", "%", "&",
                "(", ")", "[", "]", "~",
                "{", "}", "<", ">", "^",
                "`", "€", "°", "±", "×",
            ]

        case .chinese:
            return [
                "，", "。", "、", "；", "：",
                "？", "！", "……", "——", "～",
                "“", "”", "‘", "’", "·",
                "「", "」", "『", "』", "‧",
                "（", "）", "【", "】", "〃",
                "《", "》", "〈", "〉", "々",
                "〔", "〕", "〖", "〗", "〇",
                "〘", "〙", "〚", "〛", "※",
                "［", "］", "｛", "｝", "§",
                "〝", "〞", "﹏", "〰", "‖",
                "｜", "／", "＼", "＋", "－",
                "＝", "＜", "＞", "＠", "＃",
                "￥", "％", "＆", "＊", "〓",
                "｀", "＾",
            ]

        case .english:
            return [
                ".", ",", ":", ";", "?",
                "!", "\"", "'", "`", "~",
                "_", "-", "+", "=", "/",
                "\\", "|", "@", "#", "$",
                "%", "^", "&", "*", "(",
                ")", "[", "]", "{", "}",
                "<", ">", "«", "»", "‹",
                "›", "…", "–", "—", "•",
                "°", "¡", "¿", "§", "¶",
                "†", "‡", "©", "®", "™",
                "℠", "‰", "‱", "′", "″",
                "‴", "‸", "⁂", "¦",
            ]

        case .network:
            return [
                "https://", "http://", "www.", ".com", ".cn",
                ".net", ".org", ".io", ".ai", ".cc",
                ".top", ".edu", ".gov", ".hk", ".tw",
                "gmail.com", "qq.com", "163.com", "icloud.com", "outlook.com",
                "//", "/", ":", "@", "_",
                "-", "?", "=", "&", "#",
                "%", "+", "~", ".html", ".pdf",
            ]

        case .math:
            return [
                "+", "−", "±", "∓", "×",
                "÷", "=", "≠", "≈", "≡",
                "∼", "≅", "∝", "<", ">",
                "≤", "≥", "≪", "≫", "≮",
                "≯", "√", "∛", "∜", "∞",
                "∑", "∏", "∫", "∬", "∮",
                "∂", "∇", "∆", "∈", "∉",
                "∋", "⊂", "⊃", "⊆", "⊇",
                "∪", "∩", "∅", "∀", "∃",
                "¬", "∧", "∨", "⊕", "⊗",
                "⇒", "⇔", "∴", "∵", "°",
                "′", "″", "∠", "⊥", "∥",
                "△", "⊙", "π", "‰", "‱",
                "½", "⅓", "⅔", "¼", "¾",
                "⁰", "¹", "²", "³", "⁴",
                "⁵", "⁶", "⁷", "⁸", "⁹",
                "⁺", "⁻", "ⁿ", "₀", "₁",
                "₂", "₃", "₄", "₅", "₆",
                "₇", "₈", "₉", "₊", "₋",
            ]

        case .numbers:
            return [
                "①", "②", "③", "④", "⑤", "⑥", "⑦", "⑧", "⑨", "⑩",
                "⑪", "⑫", "⑬", "⑭", "⑮", "⑯", "⑰", "⑱", "⑲", "⑳",
                "❶", "❷", "❸", "❹", "❺", "❻", "❼", "❽", "❾", "❿",
                "⑴", "⑵", "⑶", "⑷", "⑸", "⑹", "⑺", "⑻", "⑼", "⑽",
                "⒈", "⒉", "⒊", "⒋", "⒌", "⒍", "⒎", "⒏", "⒐", "⒑",
                "Ⅰ", "Ⅱ", "Ⅲ", "Ⅳ", "Ⅴ", "Ⅵ", "Ⅶ", "Ⅷ", "Ⅸ", "Ⅹ", "Ⅺ", "Ⅻ",
                "ⅰ", "ⅱ", "ⅲ", "ⅳ", "ⅴ", "ⅵ", "ⅶ", "ⅷ", "ⅸ", "ⅹ", "ⅺ", "ⅻ",
                "㈠", "㈡", "㈢", "㈣", "㈤", "㈥", "㈦", "㈧", "㈨", "㈩",
                "㊀", "㊁", "㊂", "㊃", "㊄", "㊅", "㊆", "㊇", "㊈", "㊉",
                "壹", "贰", "叁", "肆", "伍", "陆", "柒", "捌", "玖", "拾",
                "佰", "仟", "万", "亿", "零",
            ]

        case .units:
            return [
                "￥", "$", "€", "£", "¥",
                "¢", "₩", "₽", "₹", "฿",
                "₫", "₴", "₦", "₱", "₪",
                "₿", "℃", "℉", "°", "‰",
                "㎡", "㎢", "㎠", "㎟", "㎥",
                "㎤", "㎞", "㎝", "㎜", "㎛",
                "㎚", "㎏", "㎎", "㎍", "㏕",
                "ml", "L", "kcal", "dB", "Hz",
                "kHz", "MHz", "GHz", "KB", "MB",
                "GB", "TB", "Ω", "μ", "Å",
            ]

        case .arrows:
            return [
                "←", "↑", "→", "↓", "↔",
                "↕", "↖", "↗", "↘", "↙",
                "⇐", "⇑", "⇒", "⇓", "⇔",
                "⇕", "⟵", "⟶", "⟷", "⟹",
                "↩", "↪", "↰", "↱", "↲",
                "↳", "↺", "↻", "⇄", "⇅",
                "⇆", "⇈", "⇊", "➜", "➔",
                "➤", "▶", "◀", "▲", "▼",
                "▷", "◁", "△", "▽",
            ]

        case .pinyin:
            return [
                "ā", "á", "ǎ", "à",
                "ē", "é", "ě", "è",
                "ī", "í", "ǐ", "ì",
                "ō", "ó", "ǒ", "ò",
                "ū", "ú", "ǔ", "ù",
                "ü", "ǖ", "ǘ", "ǚ", "ǜ",
                "ń", "ň", "ǹ", "ḿ", "ê",
            ]

        case .greek:
            return [
                "α", "β", "γ", "δ", "ε", "ζ", "η", "θ", "ι", "κ", "λ", "μ",
                "ν", "ξ", "ο", "π", "ρ", "σ", "τ", "υ", "φ", "χ", "ψ", "ω",
                "Α", "Β", "Γ", "Δ", "Ε", "Ζ", "Η", "Θ", "Ι", "Κ", "Λ", "Μ",
                "Ν", "Ξ", "Ο", "Π", "Ρ", "Σ", "Τ", "Υ", "Φ", "Χ", "Ψ", "Ω",
            ]

        case .special:
            return [
                "✓", "✔", "✗", "✘", "★",
                "☆", "✦", "✧", "♥", "♡",
                "♠", "♤", "♣", "♧", "♦",
                "♢", "♪", "♫", "♬", "♭",
                "♮", "♯", "©", "®", "™",
                "℗", "§", "¶", "※", "❖",
                "○", "●", "◎", "◉", "◇",
                "◆", "□", "■", "♀", "♂",
                "⌘", "⌥", "⇧", "⌃", "⏎",
                "⌫", "⇥", "⎋", "∞", "⁂",
            ]

        case .kaomoji:
            return [
                "(^_^)", "(≧▽≦)", "(￣▽￣)", "(*^▽^*)", "( ´▽｀)", "(˶ᵔ ᵕ ᵔ˶)",
                "(T_T)", "(>_<)", "(｡•́︿•̀｡)", "o(╥﹏╥)o", "(⊙_⊙)", "(°_°)",
                "(¬_¬)", "(•_•)", "(・・?)", "(￣ー￣)", "→_→", "←_←",
                "¯\\_(ツ)_/¯", "(╯°□°）╯", "(ง •̀_•́)ง", "(´• ω •`)", "(˘³˘)♥", "(づ｡◕‿‿◕｡)づ",
            ]
        }
    }
}

/// 符号网格逐列对齐计算：
/// - 普通单符号占 1 列，超宽网址后缀与颜文字按实际需要占 2～3 列；
/// - 当前行剩余列放不下超宽项时，用空白占位格补齐行尾；最后一行不足整行时同样补齐空白格，
///   确保每列竖线从上到下像素级对齐，且单个符号不会被拉宽成整行。
enum SymbolGridPacking {
    static func pack(items: [SymbolItem], neededWidths: [CGFloat], availableWidth: CGFloat, columns: Int) -> [SymbolGridSlot] {
        let totalWidth = floor(availableWidth)
        guard totalWidth > 0, columns > 0, items.count == neededWidths.count else { return [] }
        let unit = totalWidth / CGFloat(columns)

        func columnBoundary(_ col: Int) -> CGFloat {
            if col <= 0 { return 0 }
            if col >= columns { return totalWidth }
            return (CGFloat(col) * totalWidth / CGFloat(columns)).rounded()
        }

        func slotWidth(startCol: Int, span: Int) -> CGFloat {
            max(1, columnBoundary(startCol + span) - columnBoundary(startCol))
        }

        var slots: [SymbolGridSlot] = []
        var col = 0

        func fillRemainderWithEmpty() {
            while col < columns {
                let isEnd = (col + 1 == columns)
                slots.append(SymbolGridSlot(item: nil, span: 1, width: slotWidth(startCol: col, span: 1), isRowEnd: isEnd))
                col += 1
            }
            col = 0
        }

        for (item, width) in zip(items, neededWidths) {
            let need = min(columns, max(1, Int((width / unit).rounded(.up))))
            if col + need > columns {
                fillRemainderWithEmpty()
            }
            let isEnd = (col + need == columns)
            slots.append(SymbolGridSlot(item: item, span: need, width: slotWidth(startCol: col, span: need), isRowEnd: isEnd))
            col += need
            if col == columns {
                col = 0
            }
        }

        if col > 0 {
            fillRemainderWithEmpty()
        }

        return slots
    }
}

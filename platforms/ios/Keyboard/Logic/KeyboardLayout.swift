import CoreGraphics

/// 键盘的三个页面
enum KeyboardPage: Equatable {
    case letters
    case numbers
    case symbols
}

/// 一个键按下或上划后做什么
enum KeyAction: Equatable {
    /// 字母（小写；大小写由上档键决定）
    case letter(Character)
    /// 数字或标点，按 ASCII 定义，中文模式下经 PunctuationMap 换成中文标点
    case symbol(String)
    /// 原样上屏的字符（中英文模式都不做全角转换，如 x 上划输入半角 `.`）
    case literal(String)
    case shift
    case backspace
    case space
    case returnKey
    /// 切换输入法（地球键）
    case globe
    case toggleLanguage
    case page(KeyboardPage)
}

/// 键宽：unit 是「一个字母键宽」的倍数；flexible 平分这一行剩下的宽度
enum KeyWidth: Equatable {
    case unit(CGFloat)
    case flexible
}

struct KeySpec: Equatable {
    let action: KeyAction
    let swipeUpAction: KeyAction?
    let width: KeyWidth

    init(_ action: KeyAction, swipeUp: KeyAction? = nil, _ width: KeyWidth = .unit(1)) {
        self.action = action
        self.swipeUpAction = swipeUp
        self.width = width
    }

    /// 字母键和字符键按下时显示放大气泡
    var showsPreview: Bool {
        switch action {
        case .letter, .symbol, .literal: return true
        default: return false
        }
    }

    /// 功能键用深一点的底色
    var isFunctionKey: Bool {
        switch action {
        case .letter, .symbol, .literal, .space, .toggleLanguage: return false
        default: return true
        }
    }
}

/// 分离键盘的一行：左半区靠左拇指、右半区靠右拇指，中间留空避开折痕
struct SplitRowSpec: Equatable {
    let left: [KeySpec]
    let right: [KeySpec]
}

/// 各页面的键位。只有数据，没有界面，方便测试
enum KeyboardLayout {
    static func rows(for page: KeyboardPage, chineseMode: Bool, showsGlobe: Bool) -> [[KeySpec]] {
        switch page {
        case .letters: return letterRows(showsGlobe: showsGlobe)
        case .numbers: return numberRows(chineseMode: chineseMode, showsGlobe: showsGlobe)
        case .symbols: return symbolRows(chineseMode: chineseMode, showsGlobe: showsGlobe)
        }
    }

    /// 折叠屏展开态（如 iPhone Duo）的两侧分离式键位：按 T|Y、G|H、V|B 拆开，底排左右各配一个空格键
    static func splitRows(for page: KeyboardPage, chineseMode: Bool, showsGlobe: Bool) -> [SplitRowSpec] {
        switch page {
        case .letters: return splitLetterRows(showsGlobe: showsGlobe)
        case .numbers: return splitNumberRows(chineseMode: chineseMode, showsGlobe: showsGlobe)
        case .symbols: return splitSymbolRows(chineseMode: chineseMode, showsGlobe: showsGlobe)
        }
    }

    private static func symbols(_ list: [String], width: KeyWidth = .unit(1)) -> [KeySpec] {
        list.map { KeySpec(.symbol($0), width) }
    }

    private static func letterRows(showsGlobe: Bool) -> [[KeySpec]] {
        let row1: [KeySpec] = [
            KeySpec(.letter("q"), swipeUp: .symbol("1")),
            KeySpec(.letter("w"), swipeUp: .symbol("2")),
            KeySpec(.letter("e"), swipeUp: .symbol("3")),
            KeySpec(.letter("r"), swipeUp: .symbol("4")),
            KeySpec(.letter("t"), swipeUp: .symbol("5")),
            KeySpec(.letter("y"), swipeUp: .symbol("6")),
            KeySpec(.letter("u"), swipeUp: .symbol("7")),
            KeySpec(.letter("i"), swipeUp: .symbol("8")),
            KeySpec(.letter("o"), swipeUp: .symbol("9")),
            KeySpec(.letter("p"), swipeUp: .symbol("0")),
        ]
        let row2: [KeySpec] = [
            KeySpec(.letter("a"), swipeUp: .symbol("`")),
            KeySpec(.letter("s"), swipeUp: .symbol("/")),
            KeySpec(.letter("d"), swipeUp: .symbol(":")),
            KeySpec(.letter("f"), swipeUp: .symbol(";")),
            KeySpec(.letter("g"), swipeUp: .symbol("(")),
            KeySpec(.letter("h"), swipeUp: .symbol(")")),
            KeySpec(.letter("j"), swipeUp: .symbol("~")),
            KeySpec(.letter("k"), swipeUp: .symbol("“")),
            KeySpec(.letter("l"), swipeUp: .symbol("”")),
        ]
        let row3Letters: [KeySpec] = [
            KeySpec(.letter("z"), swipeUp: .symbol("@")),
            KeySpec(.letter("x"), swipeUp: .literal(".")),
            KeySpec(.letter("c"), swipeUp: .symbol("#")),
            KeySpec(.letter("v"), swipeUp: .symbol("\\")),
            KeySpec(.letter("b"), swipeUp: .symbol("?")),
            KeySpec(.letter("n"), swipeUp: .symbol("!")),
            KeySpec(.letter("m"), swipeUp: .symbol("...")),
        ]
        let row3 = [KeySpec(.shift, .flexible)] + row3Letters + [KeySpec(.backspace, .flexible)]

        var bottom: [KeySpec]
        if showsGlobe {
            bottom = [
                KeySpec(.page(.numbers), .unit(1.35)),
                KeySpec(.globe, .unit(1.15)),
            ]
        } else {
            bottom = [
                KeySpec(.page(.numbers), .unit(1.85)),
            ]
        }
        bottom += [
            KeySpec(.symbol(","), swipeUp: .symbol("."), .unit(1)),
            KeySpec(.space, .flexible),
            KeySpec(.toggleLanguage, .unit(1)),
            KeySpec(.returnKey, .unit(1.85)),
        ]
        return [row1, row2, row3, bottom]
    }

    private static func splitLetterRows(showsGlobe: Bool) -> [SplitRowSpec] {
        let row1Left: [KeySpec] = [
            KeySpec(.letter("q"), swipeUp: .symbol("1")),
            KeySpec(.letter("w"), swipeUp: .symbol("2")),
            KeySpec(.letter("e"), swipeUp: .symbol("3")),
            KeySpec(.letter("r"), swipeUp: .symbol("4")),
            KeySpec(.letter("t"), swipeUp: .symbol("5")),
        ]
        let row1Right: [KeySpec] = [
            KeySpec(.letter("y"), swipeUp: .symbol("6")),
            KeySpec(.letter("u"), swipeUp: .symbol("7")),
            KeySpec(.letter("i"), swipeUp: .symbol("8")),
            KeySpec(.letter("o"), swipeUp: .symbol("9")),
            KeySpec(.letter("p"), swipeUp: .symbol("0")),
        ]

        // 第二行左侧 5 键略缩至 0.95u 居中，保留标准 QWERTY 相对首行的错位感；右侧 4 键 1.0u 自然居中
        let row2Left: [KeySpec] = [
            KeySpec(.letter("a"), swipeUp: .symbol("`"), .unit(0.95)),
            KeySpec(.letter("s"), swipeUp: .symbol("/"), .unit(0.95)),
            KeySpec(.letter("d"), swipeUp: .symbol(":"), .unit(0.95)),
            KeySpec(.letter("f"), swipeUp: .symbol(";"), .unit(0.95)),
            KeySpec(.letter("g"), swipeUp: .symbol("("), .unit(0.95)),
        ]
        let row2Right: [KeySpec] = [
            KeySpec(.letter("h"), swipeUp: .symbol(")")),
            KeySpec(.letter("j"), swipeUp: .symbol("~")),
            KeySpec(.letter("k"), swipeUp: .symbol("“")),
            KeySpec(.letter("l"), swipeUp: .symbol("”")),
        ]

        let row3Left: [KeySpec] = [
            KeySpec(.shift, .flexible),
            KeySpec(.letter("z"), swipeUp: .symbol("@")),
            KeySpec(.letter("x"), swipeUp: .literal(".")),
            KeySpec(.letter("c"), swipeUp: .symbol("#")),
            KeySpec(.letter("v"), swipeUp: .symbol("\\")),
        ]
        let row3Right: [KeySpec] = [
            KeySpec(.letter("b"), swipeUp: .symbol("?")),
            KeySpec(.letter("n"), swipeUp: .symbol("!")),
            KeySpec(.letter("m"), swipeUp: .symbol("...")),
            KeySpec(.backspace, .flexible),
        ]

        let bottomLeft: [KeySpec]
        if showsGlobe {
            bottomLeft = [
                KeySpec(.page(.numbers), .unit(1.12)),
                KeySpec(.globe, .unit(0.98)),
                KeySpec(.symbol(","), swipeUp: .symbol("."), .unit(0.95)),
                KeySpec(.space, .flexible),
            ]
        } else {
            bottomLeft = [
                KeySpec(.page(.numbers), .unit(1.35)),
                KeySpec(.symbol(","), swipeUp: .symbol("."), .unit(1)),
                KeySpec(.space, .flexible),
            ]
        }
        let bottomRight: [KeySpec] = [
            KeySpec(.space, .flexible),
            KeySpec(.toggleLanguage, .unit(1.08)),
            KeySpec(.returnKey, .unit(1.62)),
        ]

        return [
            SplitRowSpec(left: row1Left, right: row1Right),
            SplitRowSpec(left: row2Left, right: row2Right),
            SplitRowSpec(left: row3Left, right: row3Right),
            SplitRowSpec(left: bottomLeft, right: bottomRight),
        ]
    }

    private static func pageBottomRow(showsGlobe: Bool) -> [KeySpec] {
        var row = [KeySpec(.page(.letters), .unit(1.6))]
        if showsGlobe { row.append(KeySpec(.globe, .unit(1.25))) }
        row += [KeySpec(.space, .flexible), KeySpec(.returnKey, .unit(2))]
        return row
    }

    private static func splitPageBottomRow(showsGlobe: Bool) -> SplitRowSpec {
        var left: [KeySpec] = [KeySpec(.page(.letters), .unit(showsGlobe ? 1.3 : 1.5))]
        if showsGlobe { left.append(KeySpec(.globe, .unit(1.05))) }
        left.append(KeySpec(.space, .flexible))
        let right: [KeySpec] = [
            KeySpec(.space, .flexible),
            KeySpec(.returnKey, .unit(1.85)),
        ]
        return SplitRowSpec(left: left, right: right)
    }

    private static func numberRows(chineseMode: Bool, showsGlobe: Bool) -> [[KeySpec]] {
        // 中文模式第三行换成 。，、？！，英文模式是 . , ? ! '
        let third = chineseMode ? [".", ",", "\\", "?", "!"] : [".", ",", "?", "!", "'"]
        return [
            symbols(["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"]),
            symbols(["-", "/", ":", ";", "(", ")", "$", "&", "@", "\""]),
            [KeySpec(.page(.symbols), .flexible)] + symbols(third, width: .unit(1.4)) + [KeySpec(.backspace, .flexible)],
            pageBottomRow(showsGlobe: showsGlobe),
        ]
    }

    private static func splitNumberRows(chineseMode: Bool, showsGlobe: Bool) -> [SplitRowSpec] {
        let third = chineseMode ? [".", ",", "\\", "?", "!"] : [".", ",", "?", "!", "'"]
        return [
            SplitRowSpec(left: symbols(["1", "2", "3", "4", "5"]), right: symbols(["6", "7", "8", "9", "0"])),
            SplitRowSpec(left: symbols(["-", "/", ":", ";", "("]), right: symbols([")", "$", "&", "@", "\""])),
            SplitRowSpec(
                left: [KeySpec(.page(.symbols), .flexible)] + symbols(Array(third.prefix(3)), width: .unit(1.12)),
                right: symbols(Array(third.suffix(2)), width: .unit(1.22)) + [KeySpec(.backspace, .flexible)]
            ),
            splitPageBottomRow(showsGlobe: showsGlobe),
        ]
    }

    private static func symbolRows(chineseMode: Bool, showsGlobe: Bool) -> [[KeySpec]] {
        [
            symbols(["[", "]", "{", "}", "#", "%", "^", "*", "+", "="]),
            symbols(["_", "\\", "|", "~", "<", ">", "€", "£", "¥", "•"]),
            [KeySpec(.page(.numbers), .flexible)] + symbols([".", ",", "?", "!", "'"], width: .unit(1.4))
                + [KeySpec(.backspace, .flexible)],
            pageBottomRow(showsGlobe: showsGlobe),
        ]
    }

    private static func splitSymbolRows(chineseMode: Bool, showsGlobe: Bool) -> [SplitRowSpec] {
        [
            SplitRowSpec(left: symbols(["[", "]", "{", "}", "#"]), right: symbols(["%", "^", "*", "+", "="])),
            SplitRowSpec(left: symbols(["_", "\\", "|", "~", "<"]), right: symbols([">", "€", "£", "¥", "•"])),
            SplitRowSpec(
                left: [KeySpec(.page(.numbers), .flexible)] + symbols([".", ",", "?"], width: .unit(1.12)),
                right: symbols(["!", "'"], width: .unit(1.22)) + [KeySpec(.backspace, .flexible)]
            ),
            splitPageBottomRow(showsGlobe: showsGlobe),
        ]
    }
}

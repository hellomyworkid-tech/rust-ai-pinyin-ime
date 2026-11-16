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

/// 各页面的键位。只有数据，没有界面，方便测试
enum KeyboardLayout {
    static func rows(for page: KeyboardPage, chineseMode: Bool, showsGlobe: Bool) -> [[KeySpec]] {
        switch page {
        case .letters: return letterRows(showsGlobe: showsGlobe)
        case .numbers: return numberRows(chineseMode: chineseMode, showsGlobe: showsGlobe)
        case .symbols: return symbolRows(chineseMode: chineseMode, showsGlobe: showsGlobe)
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

    private static func pageBottomRow(showsGlobe: Bool) -> [KeySpec] {
        var row = [KeySpec(.page(.letters), .unit(1.6))]
        if showsGlobe { row.append(KeySpec(.globe, .unit(1.25))) }
        row += [KeySpec(.space, .flexible), KeySpec(.returnKey, .unit(2))]
        return row
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

    private static func symbolRows(chineseMode: Bool, showsGlobe: Bool) -> [[KeySpec]] {
        [
            symbols(["[", "]", "{", "}", "#", "%", "^", "*", "+", "="]),
            symbols(["_", "\\", "|", "~", "<", ">", "€", "£", "¥", "•"]),
            [KeySpec(.page(.numbers), .flexible)] + symbols([".", ",", "?", "!", "'"], width: .unit(1.4))
                + [KeySpec(.backspace, .flexible)],
            pageBottomRow(showsGlobe: showsGlobe),
        ]
    }
}

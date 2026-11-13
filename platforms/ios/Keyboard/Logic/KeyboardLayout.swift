import CoreGraphics

/// 键盘的三个页面
enum KeyboardPage: Equatable {
    case letters
    case numbers
    case symbols
}

/// 一个键按下后做什么
enum KeyAction: Equatable {
    /// 字母（小写；大小写由上档键决定）
    case letter(Character)
    /// 数字或标点，按 ASCII 定义，中文模式下经 PunctuationMap 换成中文标点
    case symbol(String)
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
    let width: KeyWidth

    init(_ action: KeyAction, _ width: KeyWidth = .unit(1)) {
        self.action = action
        self.width = width
    }

    /// 字母键和字符键按下时显示放大气泡
    var showsPreview: Bool {
        switch action {
        case .letter, .symbol: return true
        default: return false
        }
    }

    /// 功能键用深一点的底色
    var isFunctionKey: Bool {
        switch action {
        case .letter, .symbol, .space: return false
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

    private static func letters(_ s: String) -> [KeySpec] {
        s.map { KeySpec(.letter($0)) }
    }

    private static func symbols(_ list: [String], width: KeyWidth = .unit(1)) -> [KeySpec] {
        list.map { KeySpec(.symbol($0), width) }
    }

    private static func letterRows(showsGlobe: Bool) -> [[KeySpec]] {
        var bottom = [KeySpec(.page(.numbers), .unit(1.25))]
        if showsGlobe { bottom.append(KeySpec(.globe, .unit(1.25))) }
        bottom += [
            KeySpec(.toggleLanguage, .unit(1.25)),
            KeySpec(.symbol(","), .unit(1)),
            KeySpec(.space, .flexible),
            KeySpec(.symbol("."), .unit(1)),
            KeySpec(.returnKey, .unit(2)),
        ]
        return [
            letters("qwertyuiop"),
            letters("asdfghjkl"),
            [KeySpec(.shift, .flexible)] + letters("zxcvbnm") + [KeySpec(.backspace, .flexible)],
            bottom,
        ]
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

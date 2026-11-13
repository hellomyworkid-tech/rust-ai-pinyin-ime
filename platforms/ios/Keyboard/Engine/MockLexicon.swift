import Foundation

/// MockEngine 用的小词表。只为界面开发和测试服务，和真实词库无关。
/// 第一个候选是默认首选；aiPick 非 nil 时，模拟「AI 结果稍后到达并把它提到首位」。
enum MockLexicon {
    struct Entry {
        /// 按音节拆好的拼音
        let syllables: [String]
        let candidates: [String]
        /// 模拟 AI 异步提升的候选（必须在 candidates 里）
        let aiPick: String?

        init(_ syllables: [String], _ candidates: [String], aiPick: String? = nil) {
            self.syllables = syllables
            self.candidates = candidates
            self.aiPick = aiPick
        }

        var key: String { syllables.joined() }
    }

    static let entries: [Entry] = [
        Entry(["ni", "hao"], ["你好", "拟好", "你号"]),
        Entry(["ni"], ["你", "呢", "泥", "尼", "拟", "逆", "腻"]),
        Entry(["hao"], ["好", "号", "毫", "豪", "耗", "浩"]),
        Entry(["wo"], ["我", "握", "窝", "卧"]),
        Entry(["wo", "men"], ["我们", "我门"]),
        Entry(["jin", "tian"], ["今天", "金田", "津田"]),
        Entry(["tian", "qi"], ["天气", "田七", "天启"]),
        Entry(["hen", "hao"], ["很好", "狠好"]),
        Entry(["jin", "tian", "tian", "qi", "hen", "hao"], ["今天天气很好", "今天天气", "今天"]),
        Entry(["shi", "yan"], ["实验", "试验", "誓言", "食盐", "失言", "试演"], aiPick: "誓言"),
        Entry(["gong", "yuan"], ["公元", "公园", "工员"], aiPick: "公园"),
        Entry(["you", "xiang"], ["油箱", "邮箱", "有向"], aiPick: "邮箱"),
        Entry(["bian", "cheng"], ["变成", "编程", "边城"], aiPick: "编程"),
        Entry(["pin", "yin"], ["拼音", "品印"]),
        Entry(["shu", "ru", "fa"], ["输入法", "输入发"]),
        Entry(["pin", "yin", "shu", "ru", "fa"], ["拼音输入法", "拼音输入", "拼音"]),
        Entry(["xie", "xie"], ["谢谢", "写写", "歇歇"]),
        Entry(["zai", "jian"], ["再见", "在建", "再建"]),
        Entry(["zhong", "guo"], ["中国", "种果"]),
        Entry(["zhong", "wen"], ["中文", "重文"]),
        Entry(["ke", "yi"], ["可以", "可疑", "刻意"]),
        Entry(["shi", "jian"], ["时间", "事件", "实践", "世间"]),
        Entry(["wen", "ti"], ["问题", "文体", "问提"]),
        Entry(["dian", "nao"], ["电脑", "店闹"]),
        Entry(["shou", "ji"], ["手机", "收集", "首集", "手记"]),
        Entry(["ming", "tian"], ["明天", "名田"]),
        Entry(["kai", "hui"], ["开会", "开回"]),
        Entry(["chi", "fan"], ["吃饭", "赤帆"]),
        Entry(["a"], ["啊", "阿", "呵"]),
        Entry(["de"], ["的", "得", "地", "德"]),
        Entry(["shi"], ["是", "时", "事", "十", "市", "使", "式", "试", "师", "石"]),
        Entry(["yi"], ["一", "以", "已", "意", "易", "亿", "衣", "医"]),
    ]

    /// 分词用的音节表：词表里出现过的音节，加上一些常见音节
    static let syllables: Set<String> = {
        var set = Set(entries.flatMap { $0.syllables })
        set.formUnion([
            "ba", "bu", "da", "dao", "dou", "ge", "he", "hui", "jiu", "lai", "le", "mei", "na", "neng",
            "qu", "ren", "shang", "shuo", "ta", "xia", "xiang", "yao", "ye", "you", "zai", "zhe", "zhi",
        ])
        return set
    }()

    /// 没在词表里的音节给一个占位候选，保证任何输入都有东西可显示
    static func fallbackCandidates(for syllable: String) -> [String] {
        ["〔\(syllable)〕"]
    }

    static func entry(for syllables: [String]) -> Entry? {
        entries.first { $0.syllables == syllables }
    }
}

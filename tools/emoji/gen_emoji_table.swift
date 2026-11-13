// 从 Unicode CLDR 的中文表情注释生成「词 → 表情」对照表 data/emoji/zh_emoji.tsv（输出已随仓库提供）。
//
// 用法（在仓库根目录，CLDR 文件放在仓库外的任意目录）：
//     swift tools/emoji/gen_emoji_table.swift <annotations.json> <annotationsDerived.json> data/emoji/zh_emoji.tsv
//
// 两个输入文件来自 github.com/unicode-org/cldr-json 的
// cldr-json/cldr-annotations-full/annotations/zh/annotations.json 和
// cldr-json/cldr-annotations-derived-full/annotationsDerived/zh/annotations.json，许可证见 data/emoji/LICENSE-CLDR。
//
// 规则：
// - 只收真正的表情（Swift 的 Unicode 属性 isEmoji，排除 ASCII、©®、肤色修饰符本身）；
//   默认按文字显示的字符补上 U+FE0F，让它按表情显示（❤ → ❤️）。
// - annotationsDerived 里只取国旗（「旗: 中国」→ 中国），肤色、性别、家庭组合等变体不收。
// - 收含汉字的关键词，和纯英文字母、至少两个字母的关键词（中文注释里本来就有的 OK、lol、SOS、ATM 等，
//   转成小写，打英文时也有表情；数字、时间不收）。每个词最多 3 个表情：名称（tts）正好是这个词的排前面，其余按码位。
// - 一个词对应的表情超过 12 个（脸、手、人这类太泛的词）就只留名称正好是它的那几个，没有就不收。
// - 排序：名称正好是这个词的在前，其次是单个字符的表情（🐱 排在 🐈‍⬛ 这类组合前面），再按码位。
// - 最后用 OVERRIDES 补上少数常用词：CLDR 没有收、或者只收了字面意思（「加油」只有 ⛽）。这几条是人工挑的，
//   排在 CLDR 的结果前面。
import Foundation

struct Entry: Decodable {
    let `default`: [String]?
    let tts: [String]?
}
struct AnnotationsFile: Decodable {
    struct Inner: Decodable { let annotations: [String: Entry] }
    let annotations: Inner
}
struct DerivedFile: Decodable {
    struct Inner: Decodable { let annotations: [String: Entry] }
    let annotationsDerived: Inner
}

func die(_ message: String) -> Never {
    FileHandle.standardError.write((message + "\n").data(using: .utf8)!)
    exit(1)
}

let args = CommandLine.arguments
guard args.count == 4 else {
    die("用法：swift tools/emoji/gen_emoji_table.swift <annotations.json> <annotationsDerived.json> <输出 tsv>")
}

func load<T: Decodable>(_ path: String, as type: T.Type) -> T {
    guard let data = FileManager.default.contents(atPath: path) else { die("读不了 \(path)") }
    do { return try JSONDecoder().decode(T.self, from: data) } catch { die("\(path) 解析失败：\(error)") }
}

let base = load(args[1], as: AnnotationsFile.self).annotations.annotations
let derived = load(args[2], as: DerivedFile.self).annotationsDerived.annotations

/// 人工补充（见文件开头）：词 -> 排在最前面的表情
let OVERRIDES: [String: [String]] = [
    "加油": ["💪", "⛽"],
    "爱": ["❤️", "😍", "🥰"],
    "爱心": ["❤️", "🫶"],
    "新年": ["🎆", "🧧", "🎉"],
    "月亮": ["🌙", "🌕"],
    "音乐": ["🎵", "🎶"],
    "庆祝": ["🎉", "🥳"],
    "点赞": ["👍"],
    "厉害": ["👍", "💪"],
    "棒": ["👍"],
    "惊讶": ["😮", "😲"],
    "么么哒": ["😘"],
]

/// 是不是要收的表情；是的话返回补全 U+FE0F 之后的写法（组合序列里每个默认按文字显示的字符后面都补，
/// 例如 🙆‍♀ 补成 🙆‍♀️，否则会显示成两个字）
func qualifiedEmoji(_ key: String) -> String? {
    let scalars = Array(key.unicodeScalars)
    guard let first = scalars.first else { return nil }
    if first.value < 0x203C { return nil }                    // ASCII、©、® 等
    if !scalars.contains(where: { $0.properties.isEmoji }) { return nil }
    if scalars.count == 1 && first.properties.isEmojiModifier { return nil }   // 单独的肤色修饰符
    if scalars.contains(where: { (0x1F3FB...0x1F3FF).contains($0.value) }) { return nil } // 带肤色的变体
    var out = String.UnicodeScalarView()
    for (i, s) in scalars.enumerated() {
        out.append(s)
        let next = i + 1 < scalars.count ? scalars[i + 1].value : nil
        let textDefault = s.properties.isEmoji && !s.properties.isEmojiPresentation && s.value > 0xFF
        if textDefault && next != 0xFE0F && next != 0x20E3 {
            out.append(Unicode.Scalar(0xFE0F)!)
        }
    }
    return String(out)
}

/// 去掉 U+FE0F 之后的字符数：越少越「单个」
func baseLength(_ emoji: String) -> Int { emoji.unicodeScalars.filter { $0.value != 0xFE0F }.count }

func hasHan(_ word: String) -> Bool {
    word.unicodeScalars.contains { (0x4E00...0x9FFF).contains($0.value) || (0x3400...0x4DBF).contains($0.value) }
}

func isFlag(_ key: String) -> Bool {
    let scalars = Array(key.unicodeScalars)
    return scalars.count == 2 && scalars.allSatisfy { (0x1F1E6...0x1F1FF).contains($0.value) }
}

/// 表里用的词：含汉字的原样；纯英文字母（至少两个）的转成小写；其余（数字、时间、单个字母）不收
func tableWord(_ raw: String) -> String? {
    let word = raw.trimmingCharacters(in: .whitespaces)
    guard !word.isEmpty, !word.contains(":") else { return nil }
    if hasHan(word) { return word }
    let letters = word.unicodeScalars.allSatisfy { (0x41...0x5A).contains($0.value) || (0x61...0x7A).contains($0.value) }
    return letters && word.count >= 2 ? word.lowercased() : nil
}

// 词 -> [(表情, 名称是否正好是这个词, 码位序)]
var table: [String: [(emoji: String, exact: Bool, order: [UInt32])]] = [:]
func add(word raw: String, emoji: String, exact: Bool, key: String) {
    guard let word = tableWord(raw) else { return }
    var list = table[word, default: []]
    if let i = list.firstIndex(where: { $0.emoji == emoji }) {
        if exact { list[i].exact = true }
    } else {
        list.append((emoji, exact, key.unicodeScalars.map(\.value)))
    }
    table[word] = list
}

for (key, entry) in base {
    guard let emoji = qualifiedEmoji(key) else { continue }
    let names = Set(entry.tts ?? [])
    for word in (entry.default ?? []) + (entry.tts ?? []) {
        add(word: word, emoji: emoji, exact: names.contains(word), key: key)
    }
}
var flags = 0
for (key, entry) in derived where isFlag(key) {
    for name in entry.tts ?? [] {
        // 「旗: 中国」
        guard let range = name.range(of: ": ") else { continue }
        let country = String(name[range.upperBound...])
        add(word: country, emoji: key, exact: true, key: key)
        flags += 1
    }
}

var lines: [String] = [
    "# 词（中文，或小写的英文）→ 表情（由 tools/emoji/gen_emoji_table.swift 从 Unicode CLDR 中文表情注释生成；Unicode License v3，见 LICENSE-CLDR）",
    "# 格式：词<TAB>表情（空格分隔，最多 3 个，最相关的在前）",
]
var dropped = 0
var output: [String: [String]] = [:]
for word in Set(table.keys).union(OVERRIDES.keys) {
    var list = table[word] ?? []
    if list.count > 12 { list = list.filter(\.exact) }
    list.sort { a, b in
        if a.exact != b.exact { return a.exact }
        let la = baseLength(a.emoji), lb = baseLength(b.emoji)
        if la != lb { return la < lb }
        return a.order.lexicographicallyPrecedes(b.order)
    }
    var emojis = OVERRIDES[word] ?? []
    for item in list where !emojis.contains(item.emoji) { emojis.append(item.emoji) }
    if emojis.isEmpty { dropped += 1; continue }
    output[word] = Array(emojis.prefix(3))
}
for word in output.keys.sorted(by: { Array($0.utf8).lexicographicallyPrecedes(Array($1.utf8)) }) {
    lines.append(word + "\t" + output[word]!.joined(separator: " "))
}
let text = lines.joined(separator: "\n") + "\n"
do { try text.write(toFile: args[3], atomically: true, encoding: .utf8) } catch { die("写不了 \(args[3])：\(error)") }
print("\(args[3])：\(lines.count - 2) 个词（国旗 \(flags) 个，太泛而不收的词 \(dropped) 个）")

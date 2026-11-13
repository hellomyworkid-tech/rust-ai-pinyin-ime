import Foundation

/// MockEngine 用的音节切分：在 MockLexicon.syllables 上做从左到右的最长匹配。
/// 真实引擎的切分复杂得多，这里只求让界面有像样的音节栏可画。
enum MockSegmenter {
    struct Piece: Equatable {
        /// 这一段的拼音；非字母（如混进来的逗号）就是那个字符本身
        let code: String
        /// 在原始输入里的字节区间 [start, end)
        let start: Int
        let end: Int
        let isPinyin: Bool
    }

    static let maxSyllableLength = 6
    /// 还没打出韵母时，两个字母的声母算一段
    private static let doubleInitials: Set<String> = ["zh", "ch", "sh"]

    static func segment(_ input: String, syllables: Set<String> = MockLexicon.syllables) -> [Piece] {
        let chars = Array(input)
        let prefixes = syllables == MockLexicon.syllables ? defaultPrefixes : allPrefixes(of: syllables)
        var pieces: [Piece] = []
        var i = 0
        var byte = 0
        while i < chars.count {
            let c = chars[i]
            if c == "'" {
                // 手动分隔符：占一个字节，不成段
                byte += 1
                i += 1
                continue
            }
            let length: Int
            let isPinyin = c.isASCII && c.isLetter
            if isPinyin {
                var run = 0
                while i + run < chars.count, chars[i + run].isASCII, chars[i + run].isLetter { run += 1 }
                let tail = String(chars[i..<(i + run)])
                if i + run == chars.count, prefixes.contains(tail) {
                    // 末尾还没打完的音节（如 nihao 打到 niha 时的 ha）留在一起
                    length = run
                } else {
                    length = longestSyllable(in: chars, from: i, limit: min(run, maxSyllableLength), syllables: syllables)
                }
            } else {
                length = 1
            }
            let code = String(chars[i..<(i + length)])
            let bytes = code.utf8.count
            pieces.append(Piece(code: code, start: byte, end: byte + bytes, isPinyin: isPinyin))
            byte += bytes
            i += length
        }
        return pieces
    }

    private static let defaultPrefixes = allPrefixes(of: MockLexicon.syllables)

    /// 所有音节的所有前缀（含音节本身）
    private static func allPrefixes(of syllables: Set<String>) -> Set<String> {
        var result = Set<String>()
        for syllable in syllables {
            for length in 1...syllable.count { result.insert(String(syllable.prefix(length))) }
        }
        return result
    }

    private static func longestSyllable(in chars: [Character], from i: Int, limit: Int, syllables: Set<String>) -> Int {
        for length in stride(from: limit, through: 1, by: -1) {
            let candidate = String(chars[i..<(i + length)])
            if syllables.contains(candidate) { return length }
        }
        if limit >= 2, doubleInitials.contains(String(chars[i..<(i + 2)])) { return 2 }
        return 1
    }
}

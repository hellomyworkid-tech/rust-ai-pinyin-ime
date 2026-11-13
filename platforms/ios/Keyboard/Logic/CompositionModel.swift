import Foundation

/// 音节栏的显示数据，从 IMEView 算出来
struct CompositionModel: Equatable {
    struct Syllable: Equatable {
        let code: String
        let isActive: Bool
        /// 这个音节的字已被用户选定（整句预览里是 pinned 的部分）
        let isPinned: Bool
    }

    let syllables: [Syllable]
    /// 整句预览，按是否选定分段
    let sentence: [IMESentencePart]

    init(view: IMEView) {
        let pinned = Self.pinnedSyllables(spans: view.syllableSpans, parts: view.fullSentenceParts)
        syllables = view.syllableSpans.enumerated().map { i, span in
            Syllable(code: span.codeKey, isActive: view.activeSyllableIndex == i, isPinned: pinned[i])
        }
        sentence = view.fullSentenceParts
    }

    /// 把整句里选定的字对到音节上：每个音节的 text 按字数依次落在整句里，
    /// 落在 pinned 段内的音节就算选定。没有字的音节（多字词挂在首音节上时）跟随前一个音节。
    static func pinnedSyllables(spans: [IMESpan], parts: [IMESentencePart]) -> [Bool] {
        var pinnedOffsets: [Range<Int>] = []
        var offset = 0
        for part in parts {
            let length = part.text.count
            if part.pinned, length > 0 { pinnedOffsets.append(offset..<(offset + length)) }
            offset += length
        }
        var result: [Bool] = []
        var position = 0
        for span in spans {
            let length = span.text.count
            if length == 0 {
                result.append(result.last ?? false)
                continue
            }
            let range = position..<(position + length)
            result.append(pinnedOffsets.contains { $0.contains(range.lowerBound) && $0.upperBound >= range.upperBound })
            position += length
        }
        return result
    }
}

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
    /// 整句预览，按是否标蓝（已选定，或当前点选 / 正在原处改写的音节对应文字）分段
    let sentence: [IMESentencePart]
    /// 滚动条应当露出的音节下标：有点选或正在改写的音节时跟随它；句中光标时跟随光标所在音节；在句尾正常输入时为 nil
    let scrollFocusIndex: Int?

    init(view: IMEView) {
        let pinned = Self.pinnedSyllables(spans: view.syllableSpans, parts: view.fullSentenceParts)
        let activeFlags = view.syllableSpans.enumerated().map { i, span in
            if view.activeSyllableIndex == i { return true }
            if let range = view.editedByteRange, range.start < range.end {
                return span.start < range.end && span.end > range.start
            }
            return false
        }
        syllables = view.syllableSpans.enumerated().map { i, span in
            Syllable(code: span.codeKey, isActive: activeFlags[i], isPinned: pinned[i])
        }
        sentence = Self.highlightedSentenceParts(
            spans: view.syllableSpans,
            parts: view.fullSentenceParts,
            activeSyllables: activeFlags
        )
        if let active = activeFlags.lastIndex(of: true) {
            scrollFocusIndex = active
        } else if let lastEnd = view.syllableSpans.last?.end, view.cursorBytePos < lastEnd {
            let pos = max(1, view.cursorBytePos)
            scrollFocusIndex = view.syllableSpans.firstIndex { $0.end >= pos }
        } else {
            scrollFocusIndex = nil
        }
    }

    /// 在整句原有的已选定分段之上，把当前点选或正在原处改写的音节（activeSyllables 为 true）
    /// 对应的预览文字也标成高亮（pinned = true，音节栏右侧按蓝色渲染）。
    static func highlightedSentenceParts(
        spans: [IMESpan],
        parts: [IMESentencePart],
        activeSyllables: [Bool]
    ) -> [IMESentencePart] {
        guard spans.count == activeSyllables.count, activeSyllables.contains(true), !parts.isEmpty else {
            return parts
        }
        var chars: [Character] = []
        var highlighted: [Bool] = []
        for part in parts {
            for ch in part.text {
                chars.append(ch)
                highlighted.append(part.pinned)
            }
        }
        guard !chars.isEmpty else { return parts }

        var cursor = 0
        var lastRange: Range<Int>?
        for (i, span) in spans.enumerated() {
            let isActive = activeSyllables[i]
            let spanChars = Array(span.text)
            if spanChars.isEmpty {
                // 多字词挂在首音节上、后续音节 text 为空时，点选后续音节也把该词标蓝
                if isActive, let prev = lastRange {
                    for idx in prev { highlighted[idx] = true }
                }
                continue
            }
            if let matchStart = Self.findSubsequence(spanChars, in: chars, from: cursor) {
                let matchEnd = matchStart + spanChars.count
                if isActive {
                    if matchStart > cursor, cursor > 0, highlighted[cursor - 1] {
                        for idx in cursor..<matchStart { highlighted[idx] = true }
                    }
                    for idx in matchStart..<matchEnd { highlighted[idx] = true }
                }
                lastRange = matchStart..<matchEnd
                cursor = matchEnd
            }
        }

        var merged: [IMESentencePart] = []
        var runStart = 0
        for idx in 1...chars.count {
            if idx == chars.count || highlighted[idx] != highlighted[runStart] {
                merged.append(IMESentencePart(text: String(chars[runStart..<idx]), pinned: highlighted[runStart]))
                runStart = idx
            }
        }
        return merged
    }

    private static func findSubsequence(
        _ needle: [Character],
        in haystack: [Character],
        from start: Int
    ) -> Int? {
        guard !needle.isEmpty, start + needle.count <= haystack.count else { return nil }
        let lastStart = haystack.count - needle.count
        for pos in start...lastStart {
            var matches = true
            for k in 0..<needle.count where haystack[pos + k] != needle[k] {
                matches = false
                break
            }
            if matches { return pos }
        }
        return nil
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

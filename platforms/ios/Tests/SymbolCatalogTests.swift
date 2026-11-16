import CoreGraphics
import CoreText
import Foundation
import XCTest
@testable import AIPinyin

final class SymbolCatalogTests: XCTestCase {
    func testAllCategoriesArePopulatedAndDeduplicated() {
        XCTAssertEqual(SymbolCategory.allCases.count, 12)
        var total = 0
        for category in SymbolCategory.allCases {
            let items = SymbolCatalog.items(for: category)
            XCTAssertFalse(items.isEmpty, "\(category.title) 分类不能为空")
            total += items.count

            var seen = Set<String>()
            for item in items {
                XCTAssertFalse(item.text.isEmpty, "\(category.title) 含有空字符串")
                XCTAssertTrue(seen.insert(item.text).inserted, "\(category.title) 存在重复符号：\(item.text)")
            }
        }
        XCTAssertGreaterThanOrEqual(total, 500, "符号总数应覆盖主流分类（当前 \(total)）")
    }

    func testEverySymbolRendersWithoutMissingGlyphTofu() {
        guard let baseFont = CTFontCreateUIFontForLanguage(.system, 20, "zh-Hans" as CFString) else {
            XCTFail("无法创建系统字体")
            return
        }

        for category in SymbolCategory.allCases {
            for item in SymbolCatalog.items(for: category) {
                let ns = item.text as NSString
                var index = 0
                while index < ns.length {
                    let range = ns.rangeOfComposedCharacterSequence(at: index)
                    let cluster = ns.substring(with: range)
                    let fallbackFont = CTFontCreateForString(baseFont, cluster as CFString, CFRange(location: 0, length: (cluster as NSString).length))
                    var utf16 = Array(cluster.utf16)
                    var glyphs = [CGGlyph](repeating: 0, count: utf16.count)
                    let mapped = CTFontGetGlyphsForCharacters(fallbackFont, &utf16, &glyphs, utf16.count)
                    XCTAssertTrue(mapped, "分类「\(category.title)」中的「\(item.text)」子串「\(cluster)」缺少字体字形（会显示为方框问号）")
                    XCTAssertFalse(glyphs.contains(0), "分类「\(category.title)」中的「\(item.text)」存在 0 号缺字字形")
                    index += range.length
                }
            }
        }
    }

    func testFullWidthBadgeDistinguishesFullAndHalfWidth() {
        XCTAssertTrue(SymbolItem("，").showsFullWidthBadge)
        XCTAssertFalse(SymbolItem(",").showsFullWidthBadge)
        XCTAssertTrue(SymbolItem("「").showsFullWidthBadge)
        XCTAssertFalse(SymbolItem("[").showsFullWidthBadge)
        XCTAssertTrue(SymbolItem("［").showsFullWidthBadge)
        XCTAssertTrue(SymbolItem("￥").showsFullWidthBadge)
        XCTAssertFalse(SymbolItem("¥").showsFullWidthBadge)
        XCTAssertFalse(SymbolItem("$").showsFullWidthBadge)
    }

    func testGridPackingAlignsEveryRowWithoutOverflow() {
        let widths: [CGFloat] = [320, 375, 393, 402.333, 430, 440, 852.667]
        for category in SymbolCategory.allCases {
            let items = SymbolCatalog.items(for: category)
            let needed = items.map { item -> CGFloat in
                item.text.count <= 3 ? 44 : CGFloat(item.text.count) * 11 + 22
            }
            for columns in [5, 8] {
                for available in widths {
                    let expectedRowWidth = floor(available)
                    let slots = SymbolGridPacking.pack(
                        items: items,
                        neededWidths: needed,
                        availableWidth: available,
                        columns: columns
                    )
                    let recovered = slots.compactMap(\.item)
                    XCTAssertEqual(recovered, items, "\(category.title) 打包后符号顺序或数量变化")

                    var rowSpan = 0
                    var rowWidth: CGFloat = 0
                    for slot in slots {
                        XCTAssertGreaterThanOrEqual(slot.span, 1)
                        XCTAssertLessThanOrEqual(slot.span, columns)
                        XCTAssertGreaterThan(slot.width, 0)
                        rowSpan += slot.span
                        rowWidth += slot.width
                        if slot.isRowEnd {
                            XCTAssertEqual(rowSpan, columns, "每行总列数必须等于 \(columns)")
                            XCTAssertEqual(rowWidth, expectedRowWidth, accuracy: 0.001, "每行像素宽度之和不能超过或少于容器整数宽度")
                            rowSpan = 0
                            rowWidth = 0
                        }
                    }
                    XCTAssertEqual(rowSpan, 0, "最后一行必须用空白格补齐并标记 isRowEnd")
                }
            }
        }
    }
}

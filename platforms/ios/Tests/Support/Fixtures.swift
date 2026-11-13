import Foundation
import XCTest

/// 读取 Tests/Fixtures 里的 JSON（Rust 状态机的真实输出）
enum Fixtures {
    private final class Marker {}

    static let all = [
        "commit_nihao_space", "commit_with_punctuation", "escape", "idle_space",
        "update_long_sentence", "update_nihao", "update_syllable_cursor",
    ]

    static func data(_ name: String, file: StaticString = #filePath, line: UInt = #line) throws -> Data {
        let bundle = Bundle(for: Marker.self)
        let url = try XCTUnwrap(bundle.url(forResource: name, withExtension: "json", subdirectory: "Fixtures"),
                                "找不到 fixture \(name).json", file: file, line: line)
        return try Data(contentsOf: url)
    }

    static func result(_ name: String) throws -> IMEResult {
        try IMEEngineJSON.decodeResult(data(name))
    }

    /// 取 update 结果里的视图
    static func view(_ name: String, file: StaticString = #filePath, line: UInt = #line) throws -> IMEView {
        guard case .update(let view) = try result(name) else {
            XCTFail("\(name) 不是 update", file: file, line: line)
            return .idle
        }
        return view
    }
}

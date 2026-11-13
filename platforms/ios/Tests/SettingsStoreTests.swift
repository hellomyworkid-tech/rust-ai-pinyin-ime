import XCTest

@MainActor
final class SettingsStoreTests: XCTestCase {
    private var directory: URL!

    override func setUpWithError() throws {
        directory = FileManager.default.temporaryDirectory.appendingPathComponent("settings-\(UUID().uuidString)")
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: directory)
    }

    private var fileURL: URL { directory.appendingPathComponent("AIPinyin/settings.json") }

    func testRoundTripThroughTemporaryDirectory() throws {
        var settings = IMESettings()
        settings.aiMode = .local
        settings.localModelID = "qwen2.5-0.5b-q4km"
        settings.fuzzy.zZh = false
        settings.fuzzy.nL = true
        settings.fuzzy.typoCorrection = false
        settings.keyClickSound = false
        settings.haptics = false
        try settings.save(to: fileURL)   // 目录不存在，要自己建
        XCTAssertEqual(IMESettings.load(from: fileURL), settings)
    }

    func testMissingOrBrokenFileGivesDefaults() throws {
        XCTAssertEqual(IMESettings.load(from: fileURL), IMESettings())
        try FileManager.default.createDirectory(at: fileURL.deletingLastPathComponent(), withIntermediateDirectories: true)
        try Data("不是 JSON".utf8).write(to: fileURL)
        XCTAssertEqual(IMESettings.load(from: fileURL), IMESettings())
    }

    /// 写进去一份非默认设置，再把 JSON 改坏一处读回来
    private func loadAfterEditing(_ edit: (inout [String: Any]) -> Void) throws -> IMESettings {
        var settings = IMESettings()
        settings.aiMode = .local
        settings.localModelID = "qwen3-0.6b-q4km"
        settings.fuzzy.nL = true
        settings.fuzzy.zZh = false
        settings.keyClickSound = false
        try settings.save(to: fileURL)
        var object = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: fileURL)) as? [String: Any])
        edit(&object)
        try JSONSerialization.data(withJSONObject: object).write(to: fileURL)
        return IMESettings.load(from: fileURL)
    }

    /// 缺一个键（以后的版本加了字段、旧文件里没有）只让那一个字段回到默认值（SW-4）
    func testMissingKeyResetsOnlyThatField() throws {
        let loaded = try loadAfterEditing { $0["haptics"] = nil }
        XCTAssertEqual(loaded.aiMode, .local)
        XCTAssertEqual(loaded.localModelID, "qwen3-0.6b-q4km")
        XCTAssertTrue(loaded.fuzzy.nL)
        XCTAssertFalse(loaded.keyClickSound)
        XCTAssertTrue(loaded.haptics, "缺的字段取默认值")
    }

    func testMissingFuzzyKeyResetsOnlyThatSwitch() throws {
        let loaded = try loadAfterEditing { object in
            var fuzzy = object["fuzzy"] as? [String: Any] ?? [:]
            fuzzy["typoCorrection"] = nil
            object["fuzzy"] = fuzzy
        }
        XCTAssertTrue(loaded.fuzzy.nL)
        XCTAssertFalse(loaded.fuzzy.zZh)
        XCTAssertTrue(loaded.fuzzy.typoCorrection)
        XCTAssertEqual(loaded.aiMode, .local)
    }

    func testUnknownEnumValueOrWrongTypeResetsOnlyThatField() throws {
        let loaded = try loadAfterEditing { object in
            object["aiMode"] = "cloud"
            object["keyClickSound"] = "yes"
            object["fuzzy"] = 42
        }
        XCTAssertEqual(loaded.aiMode, IMESettings().aiMode)
        XCTAssertTrue(loaded.keyClickSound)
        XCTAssertEqual(loaded.fuzzy, IMESettings.Fuzzy())
        XCTAssertEqual(loaded.localModelID, "qwen3-0.6b-q4km", "别的字段保留")
    }

    func testExtraKeysAndEmptyObjectAreFine() throws {
        let loaded = try loadAfterEditing { $0["somethingNew"] = [1, 2] }
        XCTAssertEqual(loaded.aiMode, .local)
        try Data("{}".utf8).write(to: fileURL)
        XCTAssertEqual(IMESettings.load(from: fileURL), IMESettings())
    }

    func testJSONUsesFieldNamesTheKeyboardReads() throws {
        var settings = IMESettings()
        settings.aiMode = .off
        try settings.save(to: fileURL)
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: fileURL)) as? [String: Any])
        XCTAssertEqual(object["aiMode"] as? String, "off")
        XCTAssertNotNil(object["fuzzy"] as? [String: Any])
        XCTAssertEqual(object["keyClickSound"] as? Bool, true)
    }

    func testEveryChangeIsWrittenImmediately() throws {
        let store = SettingsStore(initial: IMESettings(), persist: { try $0.save(to: self.fileURL) })
        store.update { $0.aiMode = .local }
        XCTAssertEqual(IMESettings.load(from: fileURL).aiMode, .local)
        store.update { $0.fuzzy.inIng = false }
        XCTAssertFalse(IMESettings.load(from: fileURL).fuzzy.inIng)
        store.update { $0.haptics = false }
        let loaded = IMESettings.load(from: fileURL)
        XCTAssertFalse(loaded.haptics)
        XCTAssertEqual(loaded, store.settings)
    }

    func testUnchangedValueDoesNotWrite() {
        var writes = 0
        let store = SettingsStore(initial: IMESettings(), persist: { _ in writes += 1 })
        store.update { $0.aiMode = IMESettings().aiMode }
        XCTAssertEqual(writes, 0)
        store.update { $0.aiMode = .off }
        XCTAssertEqual(writes, 1)
    }

    func testSaveFailureIsReported() {
        let store = SettingsStore(initial: IMESettings(), persist: { _ in throw CocoaError(.fileWriteNoPermission) })
        store.update { $0.keyClickSound = false }
        XCTAssertNotNil(store.saveError)
        // 界面上的值仍然是用户刚改的，不回滚
        XCTAssertFalse(store.settings.keyClickSound)
    }

    func testSuccessfulSaveClearsEarlierError() {
        var fail = true
        let store = SettingsStore(initial: IMESettings(), persist: { _ in if fail { throw CocoaError(.fileWriteNoPermission) } })
        store.update { $0.haptics = false }
        XCTAssertNotNil(store.saveError)
        fail = false
        store.update { $0.haptics = true }
        XCTAssertNil(store.saveError)
    }
}

import XCTest

final class ModelCatalogTests: XCTestCase {
    func testFindByID() {
        XCTAssertEqual(ModelCatalog.find("qwen2.5-0.5b-q4km")?.fileName, "qwen2.5-0.5b-instruct-q4_k_m.gguf")
        XCTAssertEqual(ModelCatalog.find("qwen3-0.6b-q4km")?.fileName, "Qwen3-0.6B-Q4_K_M.gguf")
    }

    func testFindUnknownOrNil() {
        XCTAssertNil(ModelCatalog.find(nil))
        XCTAssertNil(ModelCatalog.find("没有这个模型"))
        XCTAssertNil(ModelCatalog.find(""))
    }

    func testCatalogEntriesAreConsistent() {
        let all = ModelCatalog.all
        XCTAssertFalse(all.isEmpty)
        XCTAssertEqual(Set(all.map(\.id)).count, all.count, "id 重复")
        XCTAssertEqual(Set(all.map(\.fileName)).count, all.count, "文件名重复，两个模型会互相覆盖")
        for model in all {
            XCTAssertGreaterThan(model.bytes, 0)
            XCTAssertGreaterThan(model.residentMiB, 0)
            XCTAssertEqual(model.downloadURL.scheme, "https", "\(model.id) 必须走 https")
            XCTAssertTrue(model.fileName.hasSuffix(".gguf"))
            XCTAssertFalse(model.fileName.contains("/"))
            XCTAssertFalse(model.fileName.hasSuffix(".part"), "键盘只认正式文件名")
        }
    }

    func testSelectionPolicy() {
        // 下完第一个自动选上，已经选了别的就不动
        XCTAssertEqual(ModelSelectionPolicy.afterDownload(current: nil, completed: "a"), "a")
        XCTAssertEqual(ModelSelectionPolicy.afterDownload(current: "b", completed: "a"), "b")
        // 删掉选中的：换一个已下载的，没有了就清空；删别的不动
        XCTAssertEqual(ModelSelectionPolicy.afterRemoval(current: "a", removed: "a", downloaded: ["a", "b"]), "b")
        XCTAssertNil(ModelSelectionPolicy.afterRemoval(current: "a", removed: "a", downloaded: ["a"]))
        XCTAssertEqual(ModelSelectionPolicy.afterRemoval(current: "b", removed: "a", downloaded: ["b"]), "b")
        XCTAssertNil(ModelSelectionPolicy.afterRemoval(current: nil, removed: "a", downloaded: ["b"]))
    }
}

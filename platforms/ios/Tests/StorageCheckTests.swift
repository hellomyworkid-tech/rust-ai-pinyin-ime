import XCTest

final class StorageCheckTests: XCTestCase {
    func testRequiredIncludesMargin() {
        XCTAssertEqual(StorageCheck.requiredBytes(remaining: 500), 500 + StorageCheck.safetyMargin)
        XCTAssertEqual(StorageCheck.requiredBytes(remaining: -5), StorageCheck.safetyMargin)
    }

    func testBoundary() {
        let remaining: Int64 = 491_400_032
        let need = StorageCheck.requiredBytes(remaining: remaining)
        XCTAssertTrue(StorageCheck.hasEnoughSpace(remaining: remaining, available: need))
        XCTAssertFalse(StorageCheck.hasEnoughSpace(remaining: remaining, available: need - 1))
        XCTAssertFalse(StorageCheck.hasEnoughSpace(remaining: remaining, available: 0))
    }

    func testAvailableBytesOfRealVolume() {
        XCTAssertGreaterThan(StorageCheck.availableBytes(at: FileManager.default.temporaryDirectory), 0)
        XCTAssertGreaterThan(StorageCheck.availableBytes(), 0)
    }

    func testResumeInfoReadsBytesReceived() throws {
        let data = try PropertyListSerialization.data(
            fromPropertyList: ["NSURLSessionResumeBytesReceived": 12345], format: .binary, options: 0)
        XCTAssertEqual(ResumeInfo.bytesReceived(in: data), 12345)
        XCTAssertNil(ResumeInfo.bytesReceived(in: Data("乱码".utf8)))
    }

    func testByteFormatIsNotEmpty() {
        XCTAssertFalse(ByteFormat.string(491_400_032).isEmpty)
    }

    func testSizeMismatchMessageShowsExactBytes() {
        // 差一个字节时，换算成 MB 看不出区别，消息里必须是精确字节数
        let message = DownloadFailure.sizeMismatch(expected: 396_705_472, actual: 396_705_471).message
        XCTAssertTrue(message.contains("396,705,472"), message)
        XCTAssertTrue(message.contains("396,705,471"), message)
    }
}

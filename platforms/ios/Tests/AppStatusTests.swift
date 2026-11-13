import XCTest
import FoundationModels

final class AppStatusTests: XCTestCase {
    func testKeyboardDetection() {
        let id = "com.zengji.aipinyin.keyboard"
        XCTAssertTrue(KeyboardInstallDetector.isAdded(extensionID: id, enabledKeyboards: ["en_US@sw=QWERTY", id]))
        XCTAssertTrue(KeyboardInstallDetector.isAdded(extensionID: id, enabledKeyboards: ["com.zengji.aipinyin/com.zengji.aipinyin.keyboard"]))
        XCTAssertFalse(KeyboardInstallDetector.isAdded(extensionID: id, enabledKeyboards: ["en_US@sw=QWERTY", "emoji@sw=Emoji"]))
        XCTAssertFalse(KeyboardInstallDetector.isAdded(extensionID: id, enabledKeyboards: []))
    }

    func testAppleAvailabilityMapping() {
        XCTAssertEqual(AppleModelStatus(.available), .available)
        XCTAssertEqual(AppleModelStatus(.unavailable(.deviceNotEligible)), .deviceNotEligible)
        XCTAssertEqual(AppleModelStatus(.unavailable(.appleIntelligenceNotEnabled)), .appleIntelligenceNotEnabled)
        XCTAssertEqual(AppleModelStatus(.unavailable(.modelNotReady)), .modelNotReady)
    }

    func testEveryStatusHasAnExplanation() {
        let all: [AppleModelStatus] = [.available, .deviceNotEligible, .appleIntelligenceNotEnabled, .modelNotReady, .unknown]
        XCTAssertEqual(Set(all.map(\.explanation)).count, all.count, "每种原因要有不同的解释")
        for status in all {
            XCTAssertFalse(status.title.isEmpty)
            XCTAssertEqual(status.isAvailable, status == .available)
        }
    }

    /// 说不清状态时的解释要和键盘的实际做法一致：每次出现时检查一次，用不了就改用下载好的本地模型
    func testUnknownStatusDescribesWhatTheKeyboardActuallyDoes() {
        let text = AppleModelStatus.unknown.explanation
        XCTAssertTrue(text.contains("每次出现"), text)
        XCTAssertTrue(text.contains("本地模型"), text)
        XCTAssertFalse(text.contains("每次选词前"), "键盘不会在每次选词前检查")
    }

    @MainActor
    func testMonitorFollowsLiveStatus() {
        var current = AppleModelStatus.modelNotReady
        let monitor = AppleModelMonitor(read: { current })
        XCTAssertEqual(monitor.status, .modelNotReady)
        current = .available
        monitor.refresh()
        XCTAssertEqual(monitor.status, .available)
    }
}

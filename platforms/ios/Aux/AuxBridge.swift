import Foundation
import FoundationModels
import SwiftUI
import UIKit

private final class TaskCancellableBox: NSObject, AuxCancellable, @unchecked Sendable {
    private let cancelClosure: @Sendable () -> Void

    init(_ cancelClosure: @escaping @Sendable () -> Void) {
        self.cancelClosure = cancelClosure
    }

    func cancel() {
        cancelClosure()
    }
}

private final class PrewarmedSessionBox: NSObject, @unchecked Sendable {
    let session: LanguageModelSession

    init(session: LanguageModelSession) {
        self.session = session
    }
}

@MainActor
private final class HostingLinkController: NSObject, AuxLinkHostControlling {
    private let host: UIHostingController<HostAppLinkSurface>

    var viewController: UIViewController { host }

    init(destination: URL,
         isInteractive: Bool,
         accessibilityLabel: String,
         accessibilityValue: String,
         onPressChanged: @escaping (Bool) -> Void) {
        let surface = HostAppLinkSurface(
            destination: destination,
            isInteractive: isInteractive,
            accessibilityLabel: accessibilityLabel,
            accessibilityValue: accessibilityValue,
            onPressChanged: onPressChanged
        )
        self.host = UIHostingController(rootView: surface)
        super.init()
        host.view.backgroundColor = .clear
        host.safeAreaRegions = []
    }

    func update(destination: URL,
                isInteractive: Bool,
                accessibilityLabel: String,
                accessibilityValue: String,
                onPressChanged: @escaping (Bool) -> Void) {
        host.rootView = HostAppLinkSurface(
            destination: destination,
            isInteractive: isInteractive,
            accessibilityLabel: accessibilityLabel,
            accessibilityValue: accessibilityValue,
            onPressChanged: onPressChanged
        )
    }
}

/// 键盘扩展内公开可用的原生 SwiftUI `Link` 覆盖层：只允许打开本安装包内的 `aipinyin://` 页面。
private struct HostAppLinkSurface: View {
    var destination: URL
    var isInteractive: Bool
    var accessibilityLabel: String
    var accessibilityValue: String
    var onPressChanged: (Bool) -> Void

    var body: some View {
        Link(destination: destination) {
            Color.white.opacity(0.001)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .contentShape(Rectangle())
        }
        .buttonStyle(HostAppLinkPressStyle(onPressChanged: onPressChanged))
        .disabled(!isInteractive)
        .accessibilityLabel(accessibilityLabel)
        .accessibilityValue(accessibilityValue)
        .accessibilityAddTraits(.isButton)
        .accessibilityRemoveTraits(.isLink)
    }
}

private struct HostAppLinkPressStyle: ButtonStyle {
    var onPressChanged: (Bool) -> Void

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .onChange(of: configuration.isPressed) { _, isPressed in
                onPressChanged(isPressed)
            }
    }
}

@objc(AIPinyinAuxBridge)
public final class AuxBridge: NSObject, AuxBridgeProviding {
    public required override init() {
        super.init()
    }

    public func isAppleModelAvailable() -> Bool {
        if case .available = SystemLanguageModel.default.availability {
            return true
        }
        return false
    }

    public func prewarmAppleSession(instructions: String) -> AnyObject? {
        guard isAppleModelAvailable() else { return nil }
        let session = LanguageModelSession(model: SystemLanguageModel.default, instructions: instructions)
        session.prewarm()
        return PrewarmedSessionBox(session: session)
    }

    public func chooseWithAppleModel(prompt: String,
                                     options: [String],
                                     instructions: String,
                                     prewarmedSession: AnyObject?,
                                     completion: @escaping @Sendable (String?, NSError?) -> Void) -> AuxCancellable {
        let sessionBox = prewarmedSession as? PrewarmedSessionBox
        let task = Task {
            do {
                let session = sessionBox?.session
                    ?? LanguageModelSession(model: SystemLanguageModel.default, instructions: instructions)
                let schema = try GenerationSchema(
                    root: DynamicGenerationSchema(name: "Choice", anyOf: options),
                    dependencies: []
                )
                let response = try await session.respond(
                    to: prompt,
                    schema: schema,
                    includeSchemaInPrompt: true,
                    options: GenerationOptions(samplingMode: .greedy)
                )
                try Task.checkCancellation()
                let chosen = try response.content.value(String.self)
                completion(chosen, nil)
            } catch {
                completion(nil, error as NSError)
            }
        }
        return TaskCancellableBox {
            task.cancel()
        }
    }

    @MainActor
    public func makeLinkHost(destination: URL,
                             isInteractive: Bool,
                             accessibilityLabel: String,
                             accessibilityValue: String,
                             onPressChanged: @escaping (Bool) -> Void) -> AuxLinkHostControlling {
        HostingLinkController(
            destination: destination,
            isInteractive: isInteractive,
            accessibilityLabel: accessibilityLabel,
            accessibilityValue: accessibilityValue,
            onPressChanged: onPressChanged
        )
    }
}

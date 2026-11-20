import UIKit

/// 按键音和触感。
/// 按键音走 UIDevice.playInputClick()（需要根视图实现 UIInputViewAudioFeedback；第三方键盘只有开了完全访问才真的响）；
/// 触感只在开了完全访问且设置里打开时才用。
final class KeyFeedback {
    /// 冷启动尚未读入设置前默认静音，避免用户关了按键音时首帧极早触摸误响；读入设置后立即按真实开关同步
    private(set) var clickEnabled = false
    private(set) var hapticsEnabled = false
    private var generator: UIImpactFeedbackGenerator?
    private weak var view: UIView?

    init(view: UIView) {
        self.view = view
    }

    func configure(settings: IMESettings, hasFullAccess: Bool) {
        clickEnabled = settings.keyClickSound
        hapticsEnabled = settings.haptics && hasFullAccess
        if !hapticsEnabled { generator = nil }
    }

    func keyDown() {
        if clickEnabled { UIDevice.current.playInputClick() }
        guard hapticsEnabled, let view else { return }
        if generator == nil { generator = UIImpactFeedbackGenerator(style: .light, view: view) }
        generator?.impactOccurred()
    }
}

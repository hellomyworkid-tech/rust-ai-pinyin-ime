import UIKit

/// 按住字母键时在键上方弹出的放大气泡。全键盘只有一个，反复使用。
final class KeyPreviewView: UIView {
    private let shape = CAShapeLayer()
    private let label = UILabel()

    override init(frame: CGRect) {
        super.init(frame: frame)
        isUserInteractionEnabled = false
        isHidden = true
        layer.addSublayer(shape)
        shape.shadowOffset = CGSize(width: 0, height: 1)
        shape.shadowRadius = 2
        shape.shadowOpacity = 0.3
        label.textAlignment = .center
        label.font = .systemFont(ofSize: 32, weight: .regular)
        label.adjustsFontSizeToFitWidth = true
        label.minimumScaleFactor = 0.5
        addSubview(label)
        registerForTraitChanges([UITraitUserInterfaceStyle.self]) { (view: KeyPreviewView, _: UITraitCollection) in
            view.updateColors()
        }
        updateColors()
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    /// keyFrame 是按键在父视图坐标里的框
    func show(title: String, above keyFrame: CGRect) {
        guard let container = superview else { return }
        let balloonWidth = keyFrame.width + 22
        let balloonHeight = min(keyFrame.height * 1.15, 54)
        var balloon = CGRect(x: keyFrame.midX - balloonWidth / 2, y: keyFrame.minY - balloonHeight - 6,
                             width: balloonWidth, height: balloonHeight)
        // 贴边的键，气泡往里收，不出界
        balloon.origin.x = min(max(balloon.minX, container.bounds.minX + 1), container.bounds.maxX - balloonWidth - 1)
        balloon.origin.y = max(balloon.minY, container.bounds.minY)

        let whole = balloon.union(keyFrame)
        frame = whole
        let local = { (r: CGRect) in r.offsetBy(dx: -whole.minX, dy: -whole.minY) }
        let path = UIBezierPath(roundedRect: local(balloon), cornerRadius: 10)
        path.append(UIBezierPath(roundedRect: local(keyFrame.insetBy(dx: 0, dy: -0.5)), cornerRadius: 6))
        // 气泡和键之间的过渡
        let neck = CGRect(x: local(keyFrame).minX, y: local(balloon).maxY - 8,
                          width: keyFrame.width, height: local(keyFrame).minY - local(balloon).maxY + 16)
        path.append(UIBezierPath(rect: neck))
        shape.path = path.cgPath
        shape.shadowPath = path.cgPath
        label.text = title
        label.frame = local(balloon).insetBy(dx: 2, dy: 2)
        isHidden = false
    }

    func hide() {
        isHidden = true
    }

    private func updateColors() {
        shape.fillColor = KeyboardTheme.previewBackground.resolvedColor(with: traitCollection).cgColor
        shape.shadowColor = UIColor.black.cgColor
        label.textColor = KeyboardTheme.keyText.resolvedColor(with: traitCollection)
    }
}

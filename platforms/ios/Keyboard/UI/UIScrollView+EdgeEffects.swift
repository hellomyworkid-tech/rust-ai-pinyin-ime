import UIKit

extension UIScrollView {
    /// iOS 26 起滚动视图四边默认带模糊渐隐。候选栏、音节栏只有二三十点高，
    /// 上下两条渐隐会把整行字糊掉，所以全部关掉。
    func hideEdgeEffects() {
        topEdgeEffect.isHidden = true
        bottomEdgeEffect.isHidden = true
        leftEdgeEffect.isHidden = true
        rightEdgeEffect.isHidden = true
    }
}

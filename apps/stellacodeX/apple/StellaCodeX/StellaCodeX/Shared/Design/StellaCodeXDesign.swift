import SwiftUI

enum StellaCodeXMotion {
    static let micro = Animation.timingCurve(0.2, 0.8, 0.2, 1.0, duration: 0.14)
    static let quick = Animation.timingCurve(0.2, 0.8, 0.2, 1.0, duration: 0.16)
    static let standard = Animation.timingCurve(0.2, 0.8, 0.2, 1.0, duration: 0.19)
    static let scroll = Animation.timingCurve(0.2, 0.8, 0.2, 1.0, duration: 0.18)
}

enum StellaCodeXTypography {
    #if os(macOS)
    static let composerFontSize: CGFloat = 13
    static let composerLineHeight: CGFloat = 18
    #endif
}

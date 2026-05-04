#if os(iOS)
import SwiftUI
import UIKit

final class StellaCodeXAppDelegate: NSObject, UIApplicationDelegate {
    static var supportedOrientations: UIInterfaceOrientationMask = .allButUpsideDown

    func application(
        _ application: UIApplication,
        supportedInterfaceOrientationsFor window: UIWindow?
    ) -> UIInterfaceOrientationMask {
        Self.supportedOrientations
    }
}

@MainActor
enum IOSOrientationLock {
    private static var previousSupportedOrientations: UIInterfaceOrientationMask?
    private static var previousInterfaceOrientation: UIInterfaceOrientation?

    static func lockLandscape() {
        if previousSupportedOrientations == nil {
            previousSupportedOrientations = StellaCodeXAppDelegate.supportedOrientations
            previousInterfaceOrientation = activeWindowScene()?.interfaceOrientation
        }
        StellaCodeXAppDelegate.supportedOrientations = .landscape
        requestGeometryUpdate(.landscape)
    }

    static func unlockDefault() {
        let restoredMask = previousSupportedOrientations ?? .allButUpsideDown
        let restoredOrientation = previousInterfaceOrientation
        previousSupportedOrientations = nil
        previousInterfaceOrientation = nil

        StellaCodeXAppDelegate.supportedOrientations = restoredMask
        requestGeometryUpdate(preferredMask(for: restoredMask, previousOrientation: restoredOrientation))
    }

    private static func requestGeometryUpdate(_ mask: UIInterfaceOrientationMask) {
        guard let windowScene = activeWindowScene() else {
            return
        }

        updateSupportedInterfaceOrientations(in: windowScene)
        windowScene.requestGeometryUpdate(.iOS(interfaceOrientations: mask)) { _ in
            updateSupportedInterfaceOrientations(in: windowScene)
        }

        DispatchQueue.main.asyncAfter(deadline: .now() + 0.12) {
            updateSupportedInterfaceOrientations(in: windowScene)
        }
    }

    private static func activeWindowScene() -> UIWindowScene? {
        UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .first { $0.activationState == .foregroundActive }
    }

    private static func preferredMask(
        for supportedMask: UIInterfaceOrientationMask,
        previousOrientation: UIInterfaceOrientation?
    ) -> UIInterfaceOrientationMask {
        if let previousOrientation,
           supportedMask.contains(previousOrientation.mask) {
            return previousOrientation.mask
        }

        let deviceMask = UIDevice.current.orientation.interfaceOrientationMask
        if let deviceMask,
           supportedMask.contains(deviceMask) {
            return deviceMask
        }

        if supportedMask.contains(.portrait) {
            return .portrait
        }
        if supportedMask.contains(.landscapeRight) {
            return .landscapeRight
        }
        if supportedMask.contains(.landscapeLeft) {
            return .landscapeLeft
        }
        return supportedMask
    }

    private static func updateSupportedInterfaceOrientations(in scene: UIWindowScene) {
        for window in scene.windows {
            updateSupportedInterfaceOrientations(from: window.rootViewController)
        }
    }

    private static func updateSupportedInterfaceOrientations(from controller: UIViewController?) {
        controller?.setNeedsUpdateOfSupportedInterfaceOrientations()
        if let presented = controller?.presentedViewController {
            updateSupportedInterfaceOrientations(from: presented)
        }
    }
}

private extension UIInterfaceOrientation {
    var mask: UIInterfaceOrientationMask {
        switch self {
        case .portrait:
            .portrait
        case .portraitUpsideDown:
            .portraitUpsideDown
        case .landscapeLeft:
            .landscapeLeft
        case .landscapeRight:
            .landscapeRight
        case .unknown:
            []
        @unknown default:
            []
        }
    }
}

private extension UIDeviceOrientation {
    var interfaceOrientationMask: UIInterfaceOrientationMask? {
        switch self {
        case .portrait:
            .portrait
        case .portraitUpsideDown:
            .portraitUpsideDown
        case .landscapeLeft:
            .landscapeRight
        case .landscapeRight:
            .landscapeLeft
        default:
            nil
        }
    }
}
#endif

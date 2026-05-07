import SwiftUI
#if os(macOS)
import AppKit
#endif

enum AppAppearanceMode: String, CaseIterable, Identifiable {
    case system
    case light
    case dark

    static let storageKey = "StellaCodeX.appAppearanceMode"

    var id: String {
        rawValue
    }

    var displayName: String {
        switch self {
        case .system:
            "System"
        case .light:
            "Light"
        case .dark:
            "Dark"
        }
    }

    var detail: String {
        switch self {
        case .system:
            "Follow the system appearance."
        case .light:
            "Use light appearance."
        case .dark:
            "Use dark appearance."
        }
    }

    var colorScheme: ColorScheme? {
        switch self {
        case .system:
            nil
        case .light:
            .light
        case .dark:
            .dark
        }
    }

    #if os(macOS)
    var nsAppearance: NSAppearance? {
        switch self {
        case .system:
            nil
        case .light:
            NSAppearance(named: .aqua)
        case .dark:
            NSAppearance(named: .darkAqua)
        }
    }

    @MainActor
    func applyMacAppearance() {
        NSApp.appearance = nsAppearance
    }
    #endif
}

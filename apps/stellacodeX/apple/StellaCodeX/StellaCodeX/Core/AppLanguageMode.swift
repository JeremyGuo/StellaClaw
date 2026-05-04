import Foundation
import SwiftUI

enum AppLanguageMode: String, CaseIterable, Identifiable {
    case system
    case english
    case simplifiedChinese
    case japanese

    static let storageKey = "StellaCodeX.appLanguageMode"

    var id: String {
        rawValue
    }

    var titleKey: LocalizedStringKey {
        switch self {
        case .system:
            "System"
        case .english:
            "English"
        case .simplifiedChinese:
            "Simplified Chinese"
        case .japanese:
            "Japanese"
        }
    }

    var detailKey: LocalizedStringKey {
        switch self {
        case .system:
            "Follow the system language."
        case .english:
            "Use English."
        case .simplifiedChinese:
            "Use Simplified Chinese."
        case .japanese:
            "Use Japanese."
        }
    }

    var locale: Locale? {
        switch self {
        case .system:
            nil
        case .english:
            Locale(identifier: "en")
        case .simplifiedChinese:
            Locale(identifier: "zh-Hans")
        case .japanese:
            Locale(identifier: "ja")
        }
    }
}

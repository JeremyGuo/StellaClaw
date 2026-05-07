//
//  ContentView.swift
//  StellaCodeX
//
//  Created by Jeremy Guo on 2026/5/3.
//

import SwiftUI

struct ContentView: View {
    @Environment(\.scenePhase) private var scenePhase
    @ObservedObject var viewModel: AppViewModel
    @AppStorage(AppAppearanceMode.storageKey) private var appearanceModeRaw = AppAppearanceMode.system.rawValue
    @AppStorage(AppLanguageMode.storageKey) private var languageModeRaw = AppLanguageMode.system.rawValue

    init(viewModel: AppViewModel) {
        self.viewModel = viewModel
    }

    @MainActor
    init() {
        self.viewModel = .localDevelopment()
    }

    var body: some View {
        RootView(viewModel: viewModel)
            #if os(macOS)
            .frame(minWidth: 1040, minHeight: 680)
            .onAppear {
                appearanceMode.applyMacAppearance()
            }
            .onChange(of: appearanceModeRaw) { _, _ in
                appearanceMode.applyMacAppearance()
            }
            #endif
            .preferredColorScheme(appearanceMode.colorScheme)
            .environment(\.locale, languageMode.locale ?? .autoupdatingCurrent)
            .onChange(of: scenePhase) { _, phase in
                viewModel.handleScenePhaseChange(phase)
            }
    }

    private var appearanceMode: AppAppearanceMode {
        AppAppearanceMode(rawValue: appearanceModeRaw) ?? .system
    }

    private var languageMode: AppLanguageMode {
        AppLanguageMode(rawValue: languageModeRaw) ?? .system
    }
}

#Preview {
    ContentView()
}

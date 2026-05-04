//
//  StellaCodeXApp.swift
//  StellaCodeX
//
//  Created by Jeremy Guo on 2026/5/3.
//

import SwiftUI
#if os(macOS)
import AppKit
#endif

@main
struct StellaCodeXApp: App {
    @StateObject private var viewModel = AppViewModel.localDevelopment()

    #if os(iOS)
    @UIApplicationDelegateAdaptor(StellaCodeXAppDelegate.self) private var appDelegate
    #endif

    var body: some Scene {
        #if os(macOS)
        WindowGroup {
            ContentView(viewModel: viewModel)
        }
        .windowToolbarStyle(.unifiedCompact)

        Settings {
            MacSettingsView(viewModel: viewModel)
        }
        #else
        WindowGroup {
            ContentView(viewModel: viewModel)
        }
        #endif
    }
}

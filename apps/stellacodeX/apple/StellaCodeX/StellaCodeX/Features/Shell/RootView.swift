import SwiftUI

struct RootView: View {
    @ObservedObject var viewModel: AppViewModel

    var body: some View {
        Group {
            #if os(macOS)
            MacRootView(viewModel: viewModel)
            #else
            IOSRootView(viewModel: viewModel)
            #endif
        }
        .task {
            await viewModel.loadInitialData()
        }
    }
}

#Preview {
    RootView(viewModel: .mock())
}

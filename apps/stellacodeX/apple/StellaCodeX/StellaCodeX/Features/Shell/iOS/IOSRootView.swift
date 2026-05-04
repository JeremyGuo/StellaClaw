#if os(iOS)
import SwiftUI

struct IOSRootView: View {
    @ObservedObject var viewModel: AppViewModel
    @State private var selectedTab: IOSTab = .chats
    @State private var chatsPath = NavigationPath()

    var body: some View {
        ZStack {
            switch selectedTab {
            case .chats:
                IOSChatsTab(viewModel: viewModel, path: $chatsPath)
            case .settings:
                IOSSettingsView(viewModel: viewModel)
            }
        }
        .safeAreaInset(edge: .bottom) {
            if shouldShowTabBar {
                IOSFloatingTabBar(selectedTab: $selectedTab)
                    .transition(.move(edge: .bottom).combined(with: .opacity))
            }
        }
        .animation(.smooth(duration: 0.22), value: shouldShowTabBar)
    }

    private var shouldShowTabBar: Bool {
        selectedTab != .chats || chatsPath.isEmpty
    }
}

private enum IOSTab {
    case chats
    case settings
}

private struct IOSFloatingTabBar: View {
    @Binding var selectedTab: IOSTab

    var body: some View {
        HStack(spacing: 0) {
            tabButton(.chats, title: "Chats", systemImage: "bubble.left.and.bubble.right.fill")
            tabButton(.settings, title: "Settings", systemImage: "gearshape.fill")
        }
        .padding(5)
        .background(.ultraThinMaterial)
        .clipShape(Capsule())
        .overlay {
            Capsule()
                .strokeBorder(Color.primary.opacity(0.08))
        }
        .shadow(color: Color.black.opacity(0.18), radius: 24, x: 0, y: 10)
        .padding(.bottom, 7)
    }

    private func tabButton(_ tab: IOSTab, title: String, systemImage: String) -> some View {
        Button {
            withAnimation(.smooth(duration: 0.22)) {
                selectedTab = tab
            }
        } label: {
            VStack(spacing: 2) {
                Image(systemName: systemImage)
                    .font(.system(size: 21, weight: .semibold))
                Text(title)
                    .font(.caption2.weight(.semibold))
            }
            .foregroundStyle(selectedTab == tab ? Color.accentColor : Color.primary)
            .frame(width: 118, height: 54)
            .background {
                if selectedTab == tab {
                    Capsule()
                        .fill(PlatformColor.secondaryBackground)
                        .matchedGeometryEffect(id: "selected-ios-tab", in: namespace)
                }
            }
        }
        .buttonStyle(.plain)
    }

    @Namespace private var namespace
}

private struct IOSChatsTab: View {
    @ObservedObject var viewModel: AppViewModel
    @Binding var path: NavigationPath

    var body: some View {
        NavigationStack(path: $path) {
            IOSConversationListView(
                viewModel: viewModel,
                openConversation: { id in
                    viewModel.selectConversation(id: id)
                    path.append(id)
                }
            )
                .navigationDestination(for: ConversationSummary.ID.self) { id in
                    IOSConversationRoute(viewModel: viewModel, conversationID: id)
                }
        }
        .onChange(of: path.isEmpty) { _, isAtRoot in
            if isAtRoot {
                viewModel.clearSelectedConversationForList()
            }
        }
    }
}

private struct IOSConversationRoute: View {
    @ObservedObject var viewModel: AppViewModel
    let conversationID: ConversationSummary.ID

    var body: some View {
        IOSChatWorkspaceView(viewModel: viewModel)
            .onAppear {
                viewModel.selectConversation(id: conversationID)
            }
    }
}

#Preview {
    IOSRootView(viewModel: .mock())
}
#endif

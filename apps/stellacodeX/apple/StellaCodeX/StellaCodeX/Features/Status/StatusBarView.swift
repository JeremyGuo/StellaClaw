import SwiftUI

struct StatusBarView: View {
    @ObservedObject var viewModel: AppViewModel

    var body: some View {
        HStack(spacing: 14) {
            Label(viewModel.profile.username, systemImage: "person.crop.circle")

            Divider()
                .frame(height: 14)

            Label(viewModel.selectedConversation?.status.rawValue ?? "No Conversation", systemImage: "circle.dotted")

            Divider()
                .frame(height: 14)

            Label(viewModel.realtimeStatus, systemImage: "dot.radiowaves.left.and.right")

            Divider()
                .frame(height: 14)

            Text(viewModel.selectedConversation?.sandbox ?? "sandbox pending")
                .lineLimit(1)

            Text(viewModel.selectedConversation?.remote.isEmpty == false ? viewModel.selectedConversation?.remote ?? "" : "local")
                .lineLimit(1)

            Spacer()

            Label(viewModel.profile.connectionSummary, systemImage: viewModel.profile.connectionMode == .sshProxy ? "point.3.connected.trianglepath.dotted" : "network")
                .lineLimit(1)
        }
        .font(.caption)
        .foregroundStyle(.secondary)
        .padding(.horizontal, 14)
        .padding(.vertical, 7)
        .background(PlatformColor.statusBackground)
    }
}

#Preview {
    StatusBarView(viewModel: .mock())
}

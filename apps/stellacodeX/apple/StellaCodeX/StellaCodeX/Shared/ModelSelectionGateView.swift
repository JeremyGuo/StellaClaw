import SwiftUI

struct ModelSelectionGateView: View {
    @ObservedObject var viewModel: AppViewModel

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 10) {
                Image(systemName: "cpu")
                    .font(.system(size: 18, weight: .semibold))
                    .foregroundStyle(Color.accentColor)

                VStack(alignment: .leading, spacing: 2) {
                    Text("Choose a Model")
                        .font(.headline.weight(.semibold))
                    Text("Select a model before sending messages in this conversation.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }

            if let modelsError = viewModel.modelsError {
                Label(modelsError, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.red)
            }

            if viewModel.availableModels.isEmpty {
                Button {
                    Task {
                        await viewModel.loadModels()
                    }
                } label: {
                    Label("Load Models", systemImage: "arrow.clockwise")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(.borderedProminent)
            } else {
                LazyVStack(spacing: 8) {
                    ForEach(viewModel.availableModels.prefix(6)) { model in
                        Button {
                            viewModel.selectModelForCurrentConversation(model)
                        } label: {
                            ModelChoiceRow(model: model, isSelected: false)
                        }
                        .buttonStyle(.plain)
                    }
                }
            }
        }
        .padding(18)
        .frame(maxWidth: 480)
        .background(.regularMaterial)
        .clipShape(RoundedRectangle(cornerRadius: 22, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 22, style: .continuous)
                .strokeBorder(Color.primary.opacity(0.08))
        }
        .shadow(color: Color.black.opacity(0.14), radius: 28, x: 0, y: 16)
        .padding(.horizontal, 24)
        .task {
            if viewModel.availableModels.isEmpty {
                await viewModel.loadModels()
            }
        }
    }
}

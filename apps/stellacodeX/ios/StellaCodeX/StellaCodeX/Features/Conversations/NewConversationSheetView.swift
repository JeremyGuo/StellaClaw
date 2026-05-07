import SwiftUI

struct NewConversationSheetView: View {
    @ObservedObject var viewModel: AppViewModel
    @Binding var isPresented: Bool
    @State private var nickname = ""
    @State private var selectedModelAlias: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack {
                Text("New Chat")
                    .font(.title2.weight(.semibold))

                Spacer()

                Button {
                    isPresented = false
                } label: {
                    Image(systemName: "xmark")
                        .font(.system(size: 14, weight: .semibold))
                        .frame(width: 34, height: 34)
                        .background(PlatformColor.secondaryBackground)
                        .clipShape(Circle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Close")
            }

            VStack(alignment: .leading, spacing: 8) {
                Text("Nickname")
                    .font(.headline)

                TextField("Conversation name", text: $nickname)
                    .textFieldStyle(.roundedBorder)
            }

            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    Text("Model")
                        .font(.headline)

                    Spacer()

                    if viewModel.availableModels.isEmpty {
                        Button {
                            Task {
                                await viewModel.loadModels()
                                selectDefaultModelIfNeeded()
                            }
                        } label: {
                            Label("Reload", systemImage: "arrow.clockwise")
                        }
                        .buttonStyle(.bordered)
                    }
                }

                if let modelsError = viewModel.modelsError {
                    Label(modelsError, systemImage: "exclamationmark.triangle.fill")
                        .font(.footnote)
                        .foregroundStyle(.red)
                }

                if viewModel.availableModels.isEmpty {
                    Text("No models loaded.")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, minHeight: 116)
                        .background(PlatformColor.secondaryBackground)
                        .clipShape(RoundedRectangle(cornerRadius: 16, style: .continuous))
                } else {
                    ScrollView {
                        LazyVStack(spacing: 8) {
                            ForEach(viewModel.availableModels) { model in
                                Button {
                                    selectedModelAlias = model.alias
                                } label: {
                                    ModelChoiceRow(
                                        model: model,
                                        isSelected: selectedModelAlias == model.alias
                                    )
                                }
                                .buttonStyle(.plain)
                            }
                        }
                    }
                    .frame(maxHeight: 280)
                }
            }

            HStack {
                Spacer()

                Button("Cancel") {
                    isPresented = false
                }
                .buttonStyle(.bordered)

                Button("Create") {
                    viewModel.createConversation(
                        nickname: nickname,
                        model: selectedModelAlias
                    )
                    isPresented = false
                }
                .buttonStyle(.borderedProminent)
                .disabled(!canCreate)
            }
        }
        .padding(22)
        #if os(macOS)
        .frame(width: 460)
        #else
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(.visible)
        #endif
        .task {
            if viewModel.availableModels.isEmpty {
                await viewModel.loadModels()
            }
            selectDefaultModelIfNeeded()
        }
        .onChange(of: viewModel.availableModels) {
            selectDefaultModelIfNeeded()
        }
    }

    private var canCreate: Bool {
        !nickname.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && selectedModelAlias?.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty == false
    }

    private func selectDefaultModelIfNeeded() {
        guard selectedModelAlias == nil || !viewModel.availableModels.contains(where: { $0.alias == selectedModelAlias }) else {
            return
        }
        selectedModelAlias = viewModel.availableModels.first?.alias
    }
}

struct ModelChoiceRow: View {
    let model: ModelSummary
    let isSelected: Bool

    var body: some View {
        HStack(spacing: 12) {
            VStack(alignment: .leading, spacing: 4) {
                Text(model.alias)
                    .font(.subheadline.weight(.semibold))
                    .foregroundStyle(.primary)

                Text(model.modelName)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }

            Spacer(minLength: 12)

            Text(model.providerType)
                .font(.caption.weight(.medium))
                .foregroundStyle(.secondary)
                .padding(.horizontal, 8)
                .padding(.vertical, 4)
                .background(PlatformColor.secondaryBackground)
                .clipShape(Capsule())

            Image(systemName: isSelected ? "checkmark.circle.fill" : "circle")
                .font(.system(size: 18, weight: .semibold))
                .foregroundStyle(isSelected ? Color.accentColor : Color.secondary)
        }
        .padding(12)
        .background(isSelected ? Color.accentColor.opacity(0.12) : PlatformColor.secondaryBackground)
        .clipShape(RoundedRectangle(cornerRadius: 14, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .strokeBorder(isSelected ? Color.accentColor.opacity(0.45) : Color.primary.opacity(0.06))
        }
    }
}

//
//  StellaCodeXTests.swift
//  StellaCodeXTests
//
//  Created by Jeremy Guo on 2026/5/3.
//

import Testing
@testable import StellaCodeX

struct StellaCodeXTests {

    @Test func mockClientProvidesInitialConversations() async throws {
        let conversations = try await MockStellaAPIClient().listConversations()

        #expect(conversations.isEmpty == false)
        #expect(conversations.contains { $0.status == .running })
    }

    @MainActor
    @Test func viewModelSelectsFirstConversationOnInitialLoad() async throws {
        let viewModel = AppViewModel.mock()

        await viewModel.loadInitialData()

        #expect(viewModel.selectedConversation != nil)
        #expect(viewModel.messages.isEmpty == false)
    }

}

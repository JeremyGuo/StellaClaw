import Foundation

enum TimelineScrollIntent: Equatable {
    case none
    case alignBottom(animated: Bool)
    case preserveAnchor(TimelineEntry.ID)
    case resetToBottom
}

struct TimelineScrollTransaction: Equatable {
    let mutation: TimelineMutation
    let intent: TimelineScrollIntent

    init(
        previous: TimelineSnapshot,
        current: TimelineSnapshot,
        olderLoadAnchorID: TimelineEntry.ID?,
        canAutoAlignToBottom: Bool,
        didCompleteInitialBottomScroll: Bool
    ) {
        mutation = TimelineMutation(previous: previous, current: current)

        switch mutation {
        case .unchanged:
            intent = .none
        case .prepended:
            if let anchorID = olderLoadAnchorID ?? previous.firstEntryID {
                intent = .preserveAnchor(anchorID)
            } else {
                intent = .none
            }
        case .expanded:
            if let anchorID = olderLoadAnchorID ?? previous.firstEntryID {
                intent = .preserveAnchor(anchorID)
            } else if canAutoAlignToBottom {
                intent = .alignBottom(animated: true)
            } else {
                intent = .none
            }
        case .appended, .updated:
            if canAutoAlignToBottom {
                intent = .alignBottom(animated: true)
            } else if !didCompleteInitialBottomScroll {
                intent = .resetToBottom
            } else {
                intent = .none
            }
        case .replaced:
            intent = .resetToBottom
        }
    }
}

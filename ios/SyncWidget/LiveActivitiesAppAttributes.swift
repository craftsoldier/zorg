import ActivityKit
import Foundation

/// Must match the struct defined inside the live_activities plugin.
/// The plugin defines this internally with ContentState containing appGroupId.
/// Widget Extension needs its own copy to render the UI.
struct LiveActivitiesAppAttributes: ActivityAttributes, Identifiable {
    public typealias LiveDeliveryData = ContentState

    public struct ContentState: Codable, Hashable {
        var appGroupId: String
    }

    var id = UUID()
}

extension LiveActivitiesAppAttributes {
    func prefixedKey(_ key: String) -> String {
        return "\(id)_\(key)"
    }
}

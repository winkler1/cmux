import Foundation
import Testing
@testable import CmuxUpdater

@MainActor
@Suite struct ExternallyManagedUpdateTests {
    @Test func recognizesTheInfoPlistBuildSetting() {
        let key = UpdateController.updatesManagedExternallyInfoKey

        #expect(UpdateController.isExternallyManagedUpdateBuild(infoDictionary: [key: "YES"]))
        #expect(UpdateController.isExternallyManagedUpdateBuild(infoDictionary: [key: "yes"]))
        #expect(!UpdateController.isExternallyManagedUpdateBuild(infoDictionary: [key: "NO"]))
        #expect(!UpdateController.isExternallyManagedUpdateBuild(infoDictionary: [:]))
    }

    @Test func doesNotStartOrCheckSparkleWhenUpdatesAreManagedExternally() throws {
        let suiteName = "com.cmuxterm.updatertests.\(UUID().uuidString)"
        let defaults = try #require(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }

        let updater = FakeUpdater()
        let controller = UpdateController(
            log: NoopUpdateLog(),
            clock: SystemUpdateClock(),
            hostBundle: .main,
            defaults: defaults,
            isDevLikeBundle: false,
            updatesManagedExternally: true,
            updaterFactory: { _, _ in updater }
        )

        #expect(!controller.startUpdaterIfNeeded())
        controller.checkForUpdates()

        #expect(updater.startCallCount == 0)
        #expect(updater.checkForUpdatesCallCount == 0)
        guard case .notFound = controller.model.state else {
            Issue.record("externally managed updates should suppress Sparkle checks")
            return
        }
    }
}

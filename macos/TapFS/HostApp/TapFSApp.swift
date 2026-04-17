import Cocoa
import FileProvider

@main
struct TapFSApp {
    static func main() {
        let domain = NSFileProviderDomain(
            identifier: NSFileProviderDomainIdentifier(rawValue: "com.tapfs.v2"),
            displayName: "tapfs"
        )
        NSFileProviderManager.add(domain) { error in
            if let error = error {
                NSLog("Failed to register File Provider domain: \(error)")
            } else {
                NSLog("TapFS File Provider domain registered.")
            }
        }
        let app = NSApplication.shared
        app.setActivationPolicy(.regular)
        app.run()
    }
}

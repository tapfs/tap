//
//  FileProviderEnumerator.swift
//  TapFSProvider
//
//  Enumerates directory contents by calling tapfs_readdir through the FFI.
//

import FileProvider
import os.log

/// Enumerates children of a directory node in the VFS.
class TapFSEnumerator: NSObject, NSFileProviderEnumerator {

    private let handle: OpaquePointer
    private let nodeId: UInt64
    private let logger = Logger(subsystem: "com.tapfs.provider", category: "enumerator")

    /// - Parameters:
    ///   - handle: Opaque pointer to the Rust TapFsHandle.
    ///   - nodeId: The VFS node ID of the directory to enumerate.
    init(handle: OpaquePointer, nodeId: UInt64) {
        self.handle = handle
        self.nodeId = nodeId
        super.init()
    }

    // MARK: - NSFileProviderEnumerator

    func invalidate() {
        // Nothing to clean up -- the handle is owned by the extension.
    }

    func enumerateItems(for observer: NSFileProviderEnumerationObserver,
                        startingAt page: NSFileProviderPage) {
        logger.info("enumerateItems nodeId=\(self.nodeId)")

        let dirList = tapfs_readdir(handle, nodeId)
        defer { tapfs_free_dir_list(dirList) }

        guard dirList.count > 0, dirList.entries != nil else {
            // Empty directory or error -- just finish.
            observer.finishEnumerating(upTo: nil)
            return
        }

        var items: [NSFileProviderItem] = []
        items.reserveCapacity(Int(dirList.count))

        for i in 0 ..< Int(dirList.count) {
            let entry = dirList.entries[i]

            // Extract the name from the C string.
            guard let cName = entry.name else { continue }
            let name = String(cString: cName)

            // Fetch full attributes so we have size / permissions.
            let attr = tapfs_getattr(handle, entry.id)
            guard attr.id != 0 else { continue }

            let item = FileProviderItem(
                nodeId: attr.id,
                name: name,
                parentNodeId: nodeId,
                size: attr.size,
                isDirectory: attr.file_type == 0,
                permissions: attr.perm
            )
            items.append(item)
        }

        observer.didEnumerate(items)
        // All items delivered in a single page -- no continuation token.
        observer.finishEnumerating(upTo: nil)
    }

    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        // TapFS does not yet support incremental change tracking.
        // Signal that the client should re-enumerate from scratch.
        observer.finishEnumeratingWithError(
            NSFileProviderError(.syncAnchorExpired)
        )
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        // Return a constant anchor; change enumeration is not supported yet.
        let data = Data("tapfs-anchor-v1".utf8)
        completionHandler(NSFileProviderSyncAnchor(data))
    }
}

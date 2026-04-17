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
    private weak var ext: FileProviderExtension?
    private let logger = Logger(subsystem: "com.tapfs.provider", category: "enumerator")

    init(handle: OpaquePointer, nodeId: UInt64, extension ext: FileProviderExtension? = nil) {
        self.handle = handle
        self.nodeId = nodeId
        self.ext = ext
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

            // Skip . and .. directory entries — File Provider doesn't use them.
            if name == "." || name == ".." { continue }

            // Fetch full attributes so we have size / permissions.
            let attr = tapfs_getattr(handle, entry.id)
            guard attr.id != 0 else { continue }

            // Cache this item's name/parent so item(for:) and fetchContents work.
            ext?.cacheItem(nodeId: attr.id, name: name, parentId: nodeId)

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
        // No incremental changes — report nothing changed.
        // Returning an empty change set (instead of syncAnchorExpired) keeps
        // existing items stable and prevents fileproviderd from re-enumerating
        // and wiping items after fetchContents.
        observer.finishEnumeratingChanges(upTo: anchor, moreComing: false)
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        // Stable anchor — items don't change unless the agent writes.
        let data = Data("tapfs-anchor-v1".utf8)
        completionHandler(NSFileProviderSyncAnchor(data))
    }
}

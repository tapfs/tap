//
//  FileProviderExtension.swift
//  TapFSProvider
//
//  macOS File Provider Replicated Extension that bridges Finder / ls / cat / cp
//  operations to the Rust VirtualFs via the C FFI layer.
//

import FileProvider
import UniformTypeIdentifiers
import os.log

class FileProviderExtension: NSObject, NSFileProviderReplicatedExtension {

    let domain: NSFileProviderDomain
    private var handle: OpaquePointer?
    private let logger = Logger(subsystem: "com.tapfs.provider", category: "extension")

    // MARK: - Lifecycle

    required init(domain: NSFileProviderDomain) {
        self.domain = domain
        super.init()
        initializeRustBackend()
    }

    /// Stand up the Rust VirtualFs by loading the connector spec from a
    /// well-known location and calling `tapfs_init`.
    private func initializeRustBackend() {
        let fm = FileManager.default

        // Data directory -- also used as the Rust data_dir for drafts, versions,
        // audit logs, etc.
        let appSupport = fm.urls(for: .applicationSupportDirectory, in: .userDomainMask).first!
        let dataDir = appSupport.appendingPathComponent("tapfs")

        // Try the shared app-group container first (preferred) then fall back
        // to the per-user Application Support directory.
        var specUrl = dataDir.appendingPathComponent("connector.yaml")

        if let groupContainer = fm.containerURL(forSecurityApplicationGroupIdentifier: "group.com.tapfs") {
            let groupSpec = groupContainer.appendingPathComponent("connector.yaml")
            if fm.fileExists(atPath: groupSpec.path) {
                specUrl = groupSpec
            }
        }

        guard let specYaml = try? String(contentsOf: specUrl, encoding: .utf8) else {
            logger.error("No connector spec found at \(specUrl.path, privacy: .public)")
            return
        }

        // Ensure the data directory tree exists.
        try? fm.createDirectory(at: dataDir, withIntermediateDirectories: true)

        handle = specYaml.withCString { specPtr in
            dataDir.path.withCString { dirPtr in
                tapfs_init(specPtr, dirPtr)
            }
        }

        if handle == nil {
            logger.error("tapfs_init returned NULL -- backend failed to initialize")
        } else {
            logger.info("tapfs backend initialized successfully")
        }
    }

    func invalidate() {
        if let h = handle {
            tapfs_free(h)
            handle = nil
            logger.info("tapfs backend freed")
        }
    }

    // MARK: - Identifier helpers

    /// Convert an NSFileProviderItemIdentifier to the numeric VFS node ID.
    private func nodeId(for identifier: NSFileProviderItemIdentifier) -> UInt64? {
        switch identifier {
        case .rootContainer:
            return 1
        case .workingSet:
            return nil  // We do not support the working set.
        case .trashContainer:
            return nil  // No trash support.
        default:
            return UInt64(identifier.rawValue)
        }
    }

    // MARK: - NSFileProviderReplicatedExtension -- item metadata

    func item(for identifier: NSFileProviderItemIdentifier,
              request: NSFileProviderRequest,
              completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void) -> Progress {

        guard let h = handle else {
            completionHandler(nil, NSFileProviderError(.serverUnreachable))
            return Progress()
        }
        guard let nid = nodeId(for: identifier) else {
            completionHandler(nil, NSFileProviderError(.noSuchItem))
            return Progress()
        }

        let attr = tapfs_getattr(h, nid)
        guard attr.id != 0 else {
            completionHandler(nil, NSFileProviderError(.noSuchItem))
            return Progress()
        }

        // For items fetched by ID we may not know the name.  The root always
        // has a well-known name; for other items we perform a reverse lookup
        // by looking up the parent.  As a fallback, use the raw ID string.
        let name: String
        if nid == 1 {
            name = "TapFS"
        } else {
            name = identifier.rawValue  // placeholder -- filled in by enumerator
        }

        let item = FileProviderItem(attr: attr, name: name, parentNodeId: 1)
        completionHandler(item, nil)
        return Progress()
    }

    // MARK: - Fetching file content

    func fetchContents(for itemIdentifier: NSFileProviderItemIdentifier,
                       version requestedVersion: NSFileProviderItemVersion?,
                       request: NSFileProviderRequest,
                       completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void) -> Progress {

        guard let h = handle else {
            completionHandler(nil, nil, NSFileProviderError(.serverUnreachable))
            return Progress()
        }
        guard let nid = nodeId(for: itemIdentifier) else {
            completionHandler(nil, nil, NSFileProviderError(.noSuchItem))
            return Progress()
        }

        // Fetch attributes first so we know the total size.
        let attr = tapfs_getattr(h, nid)
        guard attr.id != 0 else {
            completionHandler(nil, nil, NSFileProviderError(.noSuchItem))
            return Progress()
        }

        // Read the full file content in one shot (capped at 256 MiB).
        let maxRead: UInt32 = min(UInt32(attr.size), 256 * 1024 * 1024)
        let ffiData = tapfs_read(h, nid, 0, maxRead)
        defer { tapfs_free_data(ffiData) }

        // Write to a temporary file so the File Provider framework can serve it.
        let tempDir = FileManager.default.temporaryDirectory
        let tempFile = tempDir.appendingPathComponent(UUID().uuidString)

        do {
            if ffiData.len > 0, let ptr = ffiData.ptr {
                let data = Data(bytes: ptr, count: Int(ffiData.len))
                try data.write(to: tempFile)
            } else {
                // Empty file.
                try Data().write(to: tempFile)
            }
        } catch {
            completionHandler(nil, nil, error)
            return Progress()
        }

        let item = FileProviderItem(
            attr: attr,
            name: itemIdentifier.rawValue,
            parentNodeId: 1
        )
        completionHandler(tempFile, item, nil)
        return Progress()
    }

    // MARK: - Creating items

    func createItem(basedOn itemTemplate: NSFileProviderItem,
                    fields: NSFileProviderItemFields,
                    contents url: URL?,
                    options: NSFileProviderCreateItemOptions = [],
                    request: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {

        guard let h = handle else {
            completionHandler(nil, [], false, NSFileProviderError(.serverUnreachable))
            return Progress()
        }

        // Resolve parent node ID.
        guard let parentNid = nodeId(for: itemTemplate.parentItemIdentifier) else {
            completionHandler(nil, [], false, NSFileProviderError(.noSuchItem))
            return Progress()
        }

        let filename = itemTemplate.filename

        // Create the node via FFI.
        let attr = filename.withCString { namePtr in
            tapfs_create(h, parentNid, namePtr)
        }

        guard attr.id != 0 else {
            completionHandler(nil, [], false, NSFileProviderError(.serverUnreachable))
            return Progress()
        }

        // If the caller provided content, write it now.
        if let contentUrl = url, let data = try? Data(contentsOf: contentUrl) {
            data.withUnsafeBytes { rawBuf in
                if let baseAddress = rawBuf.baseAddress {
                    let ptr = baseAddress.assumingMemoryBound(to: UInt8.self)
                    let _ = tapfs_write(h, attr.id, 0, ptr, UInt32(data.count))
                }
            }
        }

        // Re-fetch attributes after potential write so size is accurate.
        let updatedAttr = tapfs_getattr(h, attr.id)
        let finalAttr = updatedAttr.id != 0 ? updatedAttr : attr

        let item = FileProviderItem(
            attr: finalAttr,
            name: filename,
            parentNodeId: parentNid
        )

        completionHandler(item, [], false, nil)
        return Progress()
    }

    // MARK: - Modifying items

    func modifyItem(_ item: NSFileProviderItem,
                    baseVersion version: NSFileProviderItemVersion,
                    changedFields: NSFileProviderItemFields,
                    contents newContents: URL?,
                    options: NSFileProviderModifyItemOptions = [],
                    request: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {

        guard let h = handle else {
            completionHandler(nil, [], false, NSFileProviderError(.serverUnreachable))
            return Progress()
        }
        guard let nid = nodeId(for: item.itemIdentifier) else {
            completionHandler(nil, [], false, NSFileProviderError(.noSuchItem))
            return Progress()
        }

        // Handle rename.
        if changedFields.contains(.filename) || changedFields.contains(.parentItemIdentifier) {
            let newParentNid = nodeId(for: item.parentItemIdentifier) ?? 1
            let oldParentNid = newParentNid  // simplified -- same parent
            let oldName = item.itemIdentifier.rawValue
            let newName = item.filename
            let rc = oldName.withCString { oldPtr in
                newName.withCString { newPtr in
                    tapfs_rename(h, oldParentNid, oldPtr, newParentNid, newPtr)
                }
            }
            if rc != 0 {
                logger.warning("tapfs_rename failed for item \(nid)")
            }
        }

        // Handle content update.
        if changedFields.contains(.contents), let contentUrl = newContents,
           let data = try? Data(contentsOf: contentUrl) {
            data.withUnsafeBytes { rawBuf in
                if let baseAddress = rawBuf.baseAddress {
                    let ptr = baseAddress.assumingMemoryBound(to: UInt8.self)
                    let _ = tapfs_write(h, nid, 0, ptr, UInt32(data.count))
                }
            }
        }

        // Fetch updated attributes.
        let attr = tapfs_getattr(h, nid)
        guard attr.id != 0 else {
            completionHandler(nil, [], false, NSFileProviderError(.noSuchItem))
            return Progress()
        }

        let parentNid = nodeId(for: item.parentItemIdentifier) ?? 1
        let updatedItem = FileProviderItem(
            attr: attr,
            name: item.filename,
            parentNodeId: parentNid
        )

        completionHandler(updatedItem, [], false, nil)
        return Progress()
    }

    // MARK: - Deleting items

    func deleteItem(identifier: NSFileProviderItemIdentifier,
                    baseVersion version: NSFileProviderItemVersion,
                    options: NSFileProviderDeleteItemOptions = [],
                    request: NSFileProviderRequest,
                    completionHandler: @escaping (Error?) -> Void) -> Progress {

        guard let h = handle else {
            completionHandler(NSFileProviderError(.serverUnreachable))
            return Progress()
        }
        guard let nid = nodeId(for: identifier) else {
            completionHandler(NSFileProviderError(.noSuchItem))
            return Progress()
        }

        // We need the parent ID and name for tapfs_unlink.  The File Provider
        // framework does not give us those directly on delete, so we must look
        // up the item first via getattr.  The unlink FFI takes parent + name,
        // but we only have the node ID here.  As a workaround we use the
        // identifier's raw value as the name and assume parent = root.  A more
        // complete implementation would maintain an ID -> (parent, name) map.
        let name = identifier.rawValue
        let parentNid: UInt64 = 1  // fallback to root

        let rc = name.withCString { namePtr in
            tapfs_unlink(h, parentNid, namePtr)
        }

        if rc == 0 {
            completionHandler(nil)
        } else {
            logger.warning("tapfs_unlink failed for item \(nid)")
            completionHandler(NSFileProviderError(.serverUnreachable))
        }

        return Progress()
    }

    // MARK: - Enumeration

    func enumerator(for containerItemIdentifier: NSFileProviderItemIdentifier,
                    request: NSFileProviderRequest) throws -> NSFileProviderEnumerator {

        guard let h = handle else {
            throw NSFileProviderError(.serverUnreachable)
        }

        // Working set: return an enumerator over the root so that Spotlight
        // can index top-level items.
        if containerItemIdentifier == .workingSet {
            return TapFSEnumerator(handle: h, nodeId: 1)
        }

        guard let nid = nodeId(for: containerItemIdentifier) else {
            throw NSFileProviderError(.noSuchItem)
        }

        return TapFSEnumerator(handle: h, nodeId: nid)
    }
}

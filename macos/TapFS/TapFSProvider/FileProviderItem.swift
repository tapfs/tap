//
//  FileProviderItem.swift
//  TapFSProvider
//
//  NSFileProviderItem wrapper around VFS attributes returned by the Rust FFI.
//

import FileProvider
import UniformTypeIdentifiers

/// Wraps VFS node attributes into the NSFileProviderItem protocol required by
/// the macOS File Provider framework.
class FileProviderItem: NSObject, NSFileProviderItem {

    // MARK: - Stored properties

    /// VFS node ID (1 = root).
    let nodeId: UInt64

    /// Display name shown in Finder.
    let itemName: String

    /// VFS node ID of the parent directory.
    let parentNodeId: UInt64

    /// Size of the file content in bytes (0 for directories).
    let fileSize: UInt64

    /// Whether this node represents a directory.
    let isDirectory: Bool

    /// POSIX permission bits (e.g. 0o644).
    let permissions: UInt16

    // MARK: - Initializers

    /// Create from explicit values (used by enumerator when iterating dir entries).
    init(nodeId: UInt64, name: String, parentNodeId: UInt64, size: UInt64,
         isDirectory: Bool, permissions: UInt16) {
        self.nodeId = nodeId
        self.itemName = name
        self.parentNodeId = parentNodeId
        self.fileSize = size
        self.isDirectory = isDirectory
        self.permissions = permissions
    }

    /// Create from an FfiAttr and a known name / parent.
    convenience init(attr: FfiAttr, name: String, parentNodeId: UInt64) {
        self.init(
            nodeId: attr.id,
            name: name,
            parentNodeId: parentNodeId,
            size: attr.size,
            isDirectory: attr.file_type == 0,
            permissions: attr.perm
        )
    }

    // MARK: - NSFileProviderItem

    var itemIdentifier: NSFileProviderItemIdentifier {
        if nodeId == 1 {
            return .rootContainer
        }
        return NSFileProviderItemIdentifier(rawValue: "\(nodeId)")
    }

    var parentItemIdentifier: NSFileProviderItemIdentifier {
        if parentNodeId == 0 || parentNodeId == 1 {
            return .rootContainer
        }
        return NSFileProviderItemIdentifier(rawValue: "\(parentNodeId)")
    }

    var filename: String {
        return itemName
    }

    var contentType: UTType {
        if isDirectory {
            return .folder
        }
        // Try to derive the type from the file extension; fall back to plain text.
        let ext = (itemName as NSString).pathExtension
        if !ext.isEmpty, let uttype = UTType(filenameExtension: ext) {
            return uttype
        }
        return .plainText
    }

    var documentSize: NSNumber? {
        return isDirectory ? nil : NSNumber(value: fileSize)
    }

    var capabilities: NSFileProviderItemCapabilities {
        if isDirectory {
            return [.allowsReading, .allowsContentEnumerating]
        }
        // Check write permission bit for owner.
        let ownerWrite = (permissions & 0o200) != 0
        if ownerWrite {
            return [.allowsReading, .allowsWriting, .allowsRenaming, .allowsDeleting]
        }
        return [.allowsReading]
    }

    var itemVersion: NSFileProviderItemVersion {
        // Use the file size as a rough content version; metadata version is always 1.
        let contentData = withUnsafeBytes(of: fileSize) { Data($0) }
        let metaData = Data([0x01])
        return NSFileProviderItemVersion(contentVersion: contentData, metadataVersion: metaData)
    }
}

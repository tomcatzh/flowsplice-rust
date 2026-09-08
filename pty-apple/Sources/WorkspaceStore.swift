import Foundation
import CoreFoundation

/// Strict metadata-only format. Never persist credentials or terminal byte streams.
enum WorkspaceStore {
    enum Invalid: Error { case workspace }
    static func validated(_ value: Any, classMode: Bool) throws -> Data {
        func keys(_ object: [String:Any], _ allowed: [String]) -> Bool { Set(object.keys) == Set(allowed) }
        func boolean(_ value: Any?) -> Bool {
            guard let number = value as? NSNumber else { return false }
            return CFGetTypeID(number) == CFBooleanGetTypeID()
        }
        func string(_ value: Any?, _ limit: Int) -> Bool {
            guard let text = value as? String else { return false }
            return text.utf8.count <= limit && !text.unicodeScalars.contains { CharacterSet.controlCharacters.contains($0) }
        }
        guard let object = value as? [String:Any],
              keys(object,["version","classMode","identityWanted","homes","tabs","active","selected","page"]),
              !boolean(object["version"]), object["version"] as? Int == 1,
              boolean(object["classMode"]), object["classMode"] as? Bool == classMode,
              boolean(object["identityWanted"]),
              let homes = object["homes"] as? [[String:Any]], homes.count <= 8,
              let tabs = object["tabs"] as? [[String:Any]], tabs.count <= 64,
              let page = object["page"] as? String, ["manage","terminal","opened"].contains(page),
              object["selected"] is NSNull || string(object["selected"],128) else { throw Invalid.workspace }
        for home in homes {
            guard keys(home,["id","name","platform","relay","wanted"]),
                  string(home["id"],128), string(home["name"],128),
                  let platform = home["platform"] as? String, ["host","linux","macos"].contains(platform),
                  string(home["relay"],512), boolean(home["wanted"]) else { throw Invalid.workspace }
        }
        for tab in tabs {
            guard keys(tab,["home","session","name","mode","deleted"]),
                  string(tab["home"],128), string(tab["session"],128), string(tab["name"],256),
                  let mode = tab["mode"] as? String, ["read_only","read_write"].contains(mode),
                  boolean(tab["deleted"]) else { throw Invalid.workspace }
        }
        if !(object["active"] is NSNull) {
            guard let active = object["active"] as? [String:Any], keys(active,["home","session"]),
                  string(active["home"],128), string(active["session"],128) else { throw Invalid.workspace }
        }
        let data = try JSONSerialization.data(withJSONObject:object, options:[.sortedKeys])
        guard data.count <= 64 * 1024 else { throw Invalid.workspace }
        return data
    }
    static func save(_ value: Any, to url: URL, classMode: Bool) throws {
        let data = try validated(value, classMode:classMode)
        try FileManager.default.createDirectory(at:url.deletingLastPathComponent(), withIntermediateDirectories:true, attributes:[.posixPermissions:0o700])
        // Foundation's atomic replacement retains permissions only inconsistently;
        // write a private sibling first, then atomically rename it into place.
        let temporary = url.deletingLastPathComponent().appendingPathComponent(".workspace-" + UUID().uuidString)
        defer { try? FileManager.default.removeItem(at:temporary) }
        guard FileManager.default.createFile(atPath:temporary.path, contents:data, attributes:[.posixPermissions:0o600]) else { throw Invalid.workspace }
        if FileManager.default.fileExists(atPath:url.path) {
            _ = try FileManager.default.replaceItemAt(url, withItemAt:temporary)
        } else { try FileManager.default.moveItem(at:temporary, to:url) }
        try FileManager.default.setAttributes([.posixPermissions:0o600], ofItemAtPath:url.path)
    }
    static func load(from url: URL, classMode: Bool) -> [String:Any]? {
        guard let size = try? url.resourceValues(forKeys:[.fileSizeKey]).fileSize, size <= 64 * 1024,
              let data = try? Data(contentsOf:url), let value = try? JSONSerialization.jsonObject(with:data),
              (try? validated(value, classMode:classMode)) != nil else { return nil }
        return value as? [String:Any]
    }
}

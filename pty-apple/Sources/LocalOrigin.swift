import Foundation

/// The only origin allowed to send native actions or navigate the WebView.
enum LocalOrigin {
    static func accepts(_ url: URL?) -> Bool {
        guard let url else { return false }
        return url.scheme == "flowsplice-pty" && url.host == "app" && url.user == nil
            && url.password == nil && url.port == nil
    }
    static func resource(_ url: URL, root: URL) -> URL? {
        guard accepts(url), let path = url.path.removingPercentEncoding,
              !path.contains("\\"), !path.split(separator: "/").contains(".."),
              !path.contains("\0") else { return nil }
        let file = root.appendingPathComponent(path == "/" ? "index.html" : String(path.dropFirst()))
            .standardizedFileURL.resolvingSymlinksInPath()
        let base = root.standardizedFileURL.resolvingSymlinksInPath().path + "/"
        return file.path.hasPrefix(base) ? file : nil
    }
}

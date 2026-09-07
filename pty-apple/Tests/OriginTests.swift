import Foundation

@main
struct OriginTests {
    static func main() {
        let root = URL(fileURLWithPath:"/private/tmp/pty-origin-fixture")
        assert(LocalOrigin.accepts(URL(string:"flowsplice-pty://app/index.html")))
        for value in ["https://app/index.html", "file:///index.html", "flowsplice-pty://evil/index.html", "flowsplice-pty://user@app/index.html", "flowsplice-pty://app:80/index.html"] {
            assert(!LocalOrigin.accepts(URL(string:value)))
        }
        for value in ["flowsplice-pty://app/%2e%2e/secret", "flowsplice-pty://app/assets/%252e%252e/secret", "flowsplice-pty://app/%5csecret", "flowsplice-pty://app/%00"] {
            assert(LocalOrigin.resource(URL(string:value)!, root:root) == nil)
        }
        assert(LocalOrigin.resource(URL(string:"flowsplice-pty://app/assets/app.js")!, root:root)?.lastPathComponent == "app.js")
        print("PTY local origin checks passed")
    }
}

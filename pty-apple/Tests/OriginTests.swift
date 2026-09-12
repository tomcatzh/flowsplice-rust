import Foundation

@main
struct OriginTests {
    // Unlike assert, this evaluates every test in optimized Release builds too.
    static func check(_ condition: @autoclosure () -> Bool, file: StaticString = #file, line: UInt = #line) {
        if !condition() { fatalError("PTY test failed", file: file, line: line) }
    }
    static func main() {
        let root = URL(fileURLWithPath:"/private/tmp/pty-origin-fixture")
        check(LocalOrigin.accepts(URL(string:"flowsplice-pty://app/index.html")))
        for value in ["https://app/index.html", "file:///index.html", "flowsplice-pty://evil/index.html", "flowsplice-pty://user@app/index.html", "flowsplice-pty://app:80/index.html"] {
            check(!LocalOrigin.accepts(URL(string:value)))
        }
        for value in ["flowsplice-pty://app/%2e%2e/secret", "flowsplice-pty://app/assets/%252e%252e/secret", "flowsplice-pty://app/%5csecret", "flowsplice-pty://app/%00"] {
            check(LocalOrigin.resource(URL(string:value)!, root:root) == nil)
        }
        check(LocalOrigin.resource(URL(string:"flowsplice-pty://app/assets/app.js")!, root:root)?.lastPathComponent == "app.js")
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at:directory) }
        let file = directory.appendingPathComponent("workspace-class.json")
        let workspace: [String:Any] = ["version":1,"classMode":true,"identityWanted":true,
            "homes":[["id":"home","name":"Mac","platform":"macos","relay":"relay:7000","wanted":true]],
            "tabs":[["home":"home","session":UUID().uuidString,"name":"shell","mode":"read_write","deleted":false]],
            "active":NSNull(),"selected":"home","page":"terminal"]
        check(WorkspaceStore.load(from:file,classMode:true) == nil)
        try! WorkspaceStore.save(workspace,to:file,classMode:true)
        check(WorkspaceStore.load(from:file,classMode:true)?["page"] as? String == "terminal")
        check(WorkspaceStore.load(from:file,classMode:false) == nil)
        check((try! FileManager.default.attributesOfItem(atPath:file.path)[.posixPermissions] as! NSNumber).intValue == 0o600)
        var changed = workspace; changed["page"] = "opened"
        try! WorkspaceStore.save(changed,to:file,classMode:true)
        check(WorkspaceStore.load(from:file,classMode:true)?["page"] as? String == "opened")
        for (key,value) in [("password","secret" as Any),("version",true),("identityWanted",1),("tabs",Array(repeating:workspace["tabs"] as! [[String:Any]],count:65).flatMap{$0})] {
            var invalid = workspace; invalid[key] = value
            check((try? WorkspaceStore.save(invalid,to:file,classMode:true)) == nil)
            check(WorkspaceStore.load(from:file,classMode:true)?["page"] as? String == "opened")
        }
        var invalid = workspace
        invalid["tabs"] = [["home":"home","session":"id","name":"shell","mode":"read_write","deleted":false,"output":"secret"]]
        check((try? WorkspaceStore.validated(invalid,classMode:true)) == nil)
        invalid = workspace; invalid["homes"] = Array(repeating:(workspace["homes"] as! [[String:Any]])[0],count:9)
        check((try? WorkspaceStore.validated(invalid,classMode:true)) == nil)
        invalid = workspace
        invalid["tabs"] = [["home":"home","session":"id","name":String(repeating:"x",count:257),"mode":"read_only","deleted":false]]
        check((try? WorkspaceStore.validated(invalid,classMode:true)) == nil)
        invalid["tabs"] = Array(repeating:["home":String(repeating:"\\",count:128),"session":String(repeating:"\\",count:128),"name":String(repeating:"\\",count:256),"mode":"read_write","deleted":false] as [String:Any],count:64)
        check((try? WorkspaceStore.validated(invalid,classMode:true)) == nil)
        try! Data("not json".utf8).write(to:file)
        check(WorkspaceStore.load(from:file,classMode:true) == nil)
        print("PTY local origin and workspace checks passed")
    }
}

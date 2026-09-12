import Foundation

@main struct NativeLifecycleTests {
    static func main() {
        precondition(NativeLifecycle.shouldPoll(handle:1, demand:true, draining:false, actionUntil:0, now:0))
        precondition(!NativeLifecycle.shouldPoll(handle:1, demand:false, draining:false, actionUntil:0, now:0))
        // A stale idle snapshot cannot strand a newly admitted action.
        precondition(NativeLifecycle.shouldPoll(handle:1, demand:false, draining:false, actionUntil:10, now:0.025))
        precondition(!NativeLifecycle.shouldPoll(handle:1, demand:false, draining:false, actionUntil:10, now:10))
        precondition(NativeLifecycle.shouldPoll(handle:1, demand:true, draining:false, actionUntil:10, now:11))
        precondition(NativeLifecycle.shouldPoll(handle:1, demand:false, draining:true, actionUntil:0, now:11))
        precondition(!NativeLifecycle.shouldPoll(handle:0, demand:true, draining:true, actionUntil:20, now:11))
        precondition(!NativeLifecycle.drainExpired(started:nil, now:100))
        precondition(!NativeLifecycle.drainExpired(started:1, now:10.999))
        precondition(NativeLifecycle.drainExpired(started:1, now:11))
        precondition(NativeLifecycle.shouldReopen(closed:true, recovered:false, hasOptions:true))
        precondition(!NativeLifecycle.shouldReopen(closed:true, recovered:true, hasOptions:true))
        precondition(!NativeLifecycle.shouldReopen(closed:false, recovered:false, hasOptions:true))
        precondition(!NativeLifecycle.shouldReopen(closed:true, recovered:false, hasOptions:false))
        precondition(DeviceLabel.make("a\u{061c}\u{200b}\u{200e}\u{2060}\u{feff}b\u{200d}", fallback:"Mac") == "ab\u{200d} · PTY")
        print("Native lifecycle tests passed")
    }
}

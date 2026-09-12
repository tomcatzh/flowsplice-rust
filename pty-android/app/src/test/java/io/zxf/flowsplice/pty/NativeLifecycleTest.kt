package io.zxf.flowsplice.pty

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class NativeLifecycleTest {
    @Test fun idleAndStaleSnapshotsCannotStrandAnAdmittedAction() {
        assertTrue(NativeLifecycle.shouldPoll(1, true, false, 0, 0))
        assertFalse(NativeLifecycle.shouldPoll(1, false, false, 0, 0))
        // A queued initial idle snapshot arrives after send, before its busy/ack event.
        assertTrue(NativeLifecycle.shouldPoll(1, false, false, 10_000, 25))
        assertFalse(NativeLifecycle.shouldPoll(1, false, false, 10_000, 10_000))
        assertTrue(NativeLifecycle.shouldPoll(1, true, false, 10_000, 10_001))
        assertTrue(NativeLifecycle.shouldPoll(1, false, true, 0, 10_001))
        assertFalse(NativeLifecycle.shouldPoll(0, true, true, 20_000, 10_001))
    }
    @Test fun disconnectDeadlineIsBoundedAndReopenIsOneShot() {
        assertFalse(NativeLifecycle.drainExpired(null, 100_000))
        assertFalse(NativeLifecycle.drainExpired(100, 10_099))
        assertTrue(NativeLifecycle.drainExpired(100, 10_100))
        assertTrue(NativeLifecycle.shouldReopen(true, false, true))
        assertFalse(NativeLifecycle.shouldReopen(true, true, true))
        assertFalse(NativeLifecycle.shouldReopen(false, false, true))
        assertFalse(NativeLifecycle.shouldReopen(true, false, false))
    }
}

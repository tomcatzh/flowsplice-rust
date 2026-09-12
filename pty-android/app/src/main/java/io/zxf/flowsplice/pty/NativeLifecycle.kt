package io.zxf.flowsplice.pty

internal object NativeLifecycle {
    fun shouldPoll(handle: Long, demand: Boolean, draining: Boolean, actionUntil: Long, now: Long): Boolean =
        handle != 0L && (demand || draining || now < actionUntil)

    fun shouldReopen(closed: Boolean, recovered: Boolean, hasOptions: Boolean): Boolean = closed && !recovered && hasOptions

    fun drainExpired(started: Long?, now: Long): Boolean = started?.let { now - it >= 10_000 } ?: false
}

package io.zxf.flowsplice.pty

object NativePty {
    init { System.loadLibrary("flowsplice_pty_native") }
    external fun open(optionsJson: String): String
    external fun send(handle: Long, json: String): String
    external fun poll(handle: Long): String
    external fun close(handle: Long): String
}

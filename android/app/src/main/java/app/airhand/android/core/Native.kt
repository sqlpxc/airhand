package app.airhand.android.core

import java.nio.ByteBuffer

/**
 * Rust 核心（android/native，libairhand_android.so）的 JNI 接口。
 *
 * * 句柄：`ctlStart` / `hostStart` 返回，之后的调用都带着它，`ctlStop` / `hostStop` 之后作废；
 * * 命令和事件是 JSON 字符串（格式见 Rust 端的 `ctl::Cmd`、`host::Op` 和各个事件）；
 * * `poll*` 会阻塞（最多 `timeoutMs`），只能在后台线程调用；`ctlCall` 的 connect 也会阻塞到有结果。
 */
object Native {
    init {
        System.loadLibrary("airhand_android")
    }

    @JvmStatic external fun init(logFilter: String)
    @JvmStatic external fun protocolVersion(): Int

    /** 校验证书指纹：合法返回 null，否则返回原因（英文） */
    @JvmStatic external fun checkFingerprint(fp: String): String?

    // ── 控制端 ──
    @JvmStatic external fun ctlStart(configJson: String): Long
    @JvmStatic external fun ctlStop(handle: Long)
    @JvmStatic external fun ctlPollEvent(handle: Long, timeoutMs: Int): String?
    @JvmStatic external fun ctlCall(handle: Long, json: String): String
    @JvmStatic external fun ctlPollSession(handle: Long, sid: Int, timeoutMs: Int): ByteArray?
    @JvmStatic external fun ctlAck(handle: Long, sid: Int, seq: Int)

    // ── 被控端 ──
    @JvmStatic external fun hostStart(configJson: String): Long
    @JvmStatic external fun hostStop(handle: Long)
    @JvmStatic external fun hostPollEvent(handle: Long, timeoutMs: Int): String?
    @JvmStatic external fun hostCall(handle: Long, json: String)
    @JvmStatic external fun hostTick(handle: Long): String?
    @JvmStatic external fun hostWaitWindow(handle: Long, timeoutMs: Int): Boolean

    /** `flags`：bit0 关键帧，bit1 参数集；`codec`：[Codec.id] */
    @JvmStatic external fun hostVideo(
        handle: Long, buf: ByteBuffer, offset: Int, len: Int, flags: Int,
        captureUs: Long, codec: Int, width: Int, height: Int,
    )

    @JvmStatic external fun hostAudio(handle: Long, buf: ByteBuffer, offset: Int, len: Int, captureUs: Long)

    // ── 剪贴板里的文件（见 Rust 端的 files 和 ClipFiles）：key 是对方的一个公告 ──
    /** 列出目录（`path` 是 JSON 数组）：`{"entries":[...]}` 或 `{"error":"..."}`。阻塞 */
    @JvmStatic external fun filesList(key: Long, path: String, recursive: Boolean): String
    /** 打开对方的一个文件（读的时候才取）：文件的编号，公告不在了是 0。`mtime` 不知道时是 -1 */
    @JvmStatic external fun filesOpen(key: Long, path: String, size: Long, mtime: Long): Long
    /** 从 offset 开始读进 buf 的前 len 个字节：读了多少（0 是到末尾了），出错是负数。阻塞 */
    @JvmStatic external fun filesRead(file: Long, offset: Long, buf: ByteArray, len: Int): Int
    @JvmStatic external fun filesClose(file: Long)
    /** 放进手机的剪贴板了（放不了也要说：对方在等） */
    @JvmStatic external fun filesReady(key: Long)
}

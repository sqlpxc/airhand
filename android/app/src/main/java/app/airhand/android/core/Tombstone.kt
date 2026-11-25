package app.airhand.android.core

/**
 * 原生崩溃的 tombstone（Android 12+ 的 `ApplicationExitInfo.getTraceInputStream()` 给的 protobuf，格式见 AOSP 的
 * system/core/debuggerd/proto/tombstone.proto）：取出看得懂的部分，排成和 logcat 里 debuggerd 打的差不多的样子——
 * 信号、abort 消息、原因、崩溃的那个线程和它的调用栈。寄存器、内存映射、打开的文件、别的线程都不要。
 *
 * 只认用得到的几个字段，别的跳过。读的时候有上限（见 [CrashLog]），文件可能被截断：用已经读到的。
 * 本应用的 .so 是去掉了符号的，调用栈里只有偏移（`pc`）和 BuildId，要用同一次构建没去符号的 .so 才能对上函数名
 */
internal object Tombstone {
    /** 调用栈最多列这么多层 */
    const val MAX_FRAMES = 64

    /** 看得懂的部分；不是 tombstone、什么也没读到时为 null */
    fun summary(data: ByteArray): String? = try {
        decode(data)
    } catch (e: RuntimeException) {
        null
    }

    private class Frame(
        val relPc: Long,
        val function: String,
        val offset: Long,
        val file: String,
        val mapOffset: Long,
        val buildId: String,
    )

    private class Backtrace(val name: String, val frames: List<Frame>, val more: Int)

    private fun decode(data: ByteArray): String? {
        var arch = 0
        var timestamp = ""
        var pid = 0L
        var tid = 0L
        var command = ""
        var signal: String? = null
        var abort = ""
        val causes = ArrayList<String>()
        // 线程是 map<uint32, Thread>：每一项是 {1: tid, 2: Thread}，先记下来，读完再挑崩溃的那个
        val threads = ArrayList<Pair<Long, ProtoReader>>()
        val r = ProtoReader(data)
        var cut = false
        try {
            while (r.next()) {
                when (r.field) {
                    1 -> arch = r.varint.toInt()
                    4 -> timestamp = r.string()
                    5 -> pid = r.varint
                    6 -> tid = r.varint
                    9 -> if (command.isEmpty()) command = r.string()
                    10 -> signal = signal(r.message(), arch)
                    14 -> abort = r.string()
                    15 -> r.message().let { c ->
                        while (c.next()) if (c.field == 1) c.string().takeIf { it.isNotBlank() }?.let { causes += it }
                    }
                    16 -> r.message().let { e ->
                        var key = 0L
                        var value: ProtoReader? = null
                        while (e.next()) {
                            when (e.field) {
                                1 -> key = e.varint
                                2 -> value = e.message()
                            }
                        }
                        value?.let { threads += key to it }
                    }
                }
            }
        } catch (e: IllegalArgumentException) {
            // 读到一半不对了（多半是被截断）：前面读到的照样用
            cut = true
        }
        if (signal == null && abort.isEmpty() && threads.isEmpty()) return null

        val crashed = threads.firstOrNull { it.first == tid } ?: threads.firstOrNull()
        val thread = crashed?.let { runCatching { thread(it.second) }.getOrNull() }
        val width = if (arch == ARM64 || arch == X86_64 || arch == RISCV64) 16 else 8
        return buildString {
            if (timestamp.isNotEmpty()) appendLine("Timestamp: $timestamp")
            append("pid: $pid, tid: $tid, name: ${thread?.name ?: "?"}")
            if (command.isNotEmpty()) append("  >>> $command <<<")
            appendLine()
            signal?.let { appendLine(it) }
            if (abort.isNotEmpty()) appendLine("Abort message: '$abort'")
            causes.forEach { appendLine("Cause: $it") }
            when {
                crashed == null -> appendLine("(no threads in the tombstone)")
                thread == null -> appendLine("(the backtrace couldn't be read)")
                else -> {
                    appendLine("backtrace:")
                    thread.frames.forEachIndexed { i, f -> appendLine(frame(i, f, width)) }
                    if (thread.more > 0) appendLine("      … ${thread.more} more frames")
                }
            }
            if (cut) appendLine("(the tombstone was cut short; this is what could be read)")
        }.trimEnd()
    }

    /** debuggerd 的样子：`signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x0000000000000000` */
    private fun signal(p: ProtoReader, arch: Int): String {
        var number = 0
        var name = ""
        var code = 0
        var codeName = ""
        var hasSender = false
        var senderUid = 0
        var senderPid = 0
        var hasFault = false
        var fault = 0L
        while (p.next()) {
            when (p.field) {
                1 -> number = p.varint.toInt()
                2 -> name = p.string()
                3 -> code = p.varint.toInt()
                4 -> codeName = p.string()
                5 -> hasSender = p.varint != 0L
                6 -> senderUid = p.varint.toInt()
                7 -> senderPid = p.varint.toInt()
                8 -> hasFault = p.varint != 0L
                9 -> fault = p.varint
            }
        }
        val width = if (arch == ARM64 || arch == X86_64 || arch == RISCV64) 16 else 8
        val sender = if (hasSender) " from pid $senderPid, uid $senderUid" else ""
        val addr = if (hasFault) "0x${hex(fault, width)}" else "--------"
        return "signal $number ($name), code $code ($codeName$sender), fault addr $addr"
    }

    private fun thread(p: ProtoReader): Backtrace {
        var name = ""
        val frames = ArrayList<Frame>()
        var more = 0
        while (p.next()) {
            when (p.field) {
                2 -> name = p.string()
                4 -> if (frames.size < MAX_FRAMES) frames += frame(p.message()) else more++
            }
        }
        return Backtrace(name, frames, more)
    }

    private fun frame(p: ProtoReader): Frame {
        var relPc = 0L
        var function = ""
        var offset = 0L
        var file = ""
        var mapOffset = 0L
        var buildId = ""
        while (p.next()) {
            when (p.field) {
                1 -> relPc = p.varint
                4 -> function = p.string()
                5 -> offset = p.varint
                6 -> file = p.string()
                7 -> mapOffset = p.varint
                8 -> buildId = p.string()
            }
        }
        return Frame(relPc, function, offset, file, mapOffset, buildId)
    }

    /** debuggerd 的样子：`      #00 pc 000000000004f2c8  /path/libx.so (offset 0x1000) (func+12) (BuildId: …)` */
    private fun frame(i: Int, f: Frame, width: Int): String = buildString {
        append("      #").append(i.toString().padStart(2, '0'))
        append(" pc ").append(hex(f.relPc, width))
        append("  ").append(f.file.ifEmpty { "<unknown>" })
        if (f.mapOffset != 0L) append(" (offset 0x").append(java.lang.Long.toHexString(f.mapOffset)).append(')')
        if (f.function.isNotEmpty()) {
            append(" (").append(f.function)
            if (f.offset != 0L) append('+').append(f.offset)
            append(')')
        }
        if (f.buildId.isNotEmpty()) append(" (BuildId: ").append(f.buildId).append(')')
    }

    private fun hex(v: Long, width: Int) = java.lang.Long.toHexString(v).padStart(width, '0')

    // enum Architecture
    private const val ARM64 = 1
    private const val X86_64 = 3
    private const val RISCV64 = 4
}

/**
 * 最小的 protobuf 读取器：一个字段一个字段地读（[next]），按字段号取值。格式不对、越界时抛 IllegalArgumentException。
 * 类型对不上时取值给空的（`string()` 为 ""、`message()` 为空消息），不抛
 */
internal class ProtoReader(private val b: ByteArray, private var pos: Int = 0, private val end: Int = b.size) {
    var field = 0
        private set
    var wire = 0
        private set

    /** varint（wire 0）、fixed64（wire 1）、fixed32（wire 5）的值 */
    var varint = 0L
        private set
    private var start = 0
    private var stop = 0

    /** 读下一个字段；读完了返回 false */
    fun next(): Boolean {
        if (pos >= end) return false
        val key = readVarint()
        field = (key ushr 3).toInt()
        wire = (key and 7).toInt()
        require(field > 0) { "field 0" }
        start = 0
        stop = 0
        when (wire) {
            0 -> varint = readVarint()
            1 -> varint = fixed(8)
            2 -> {
                val len = readVarint()
                require(len >= 0 && len <= end - pos) { "length $len past the end" }
                start = pos
                stop = pos + len.toInt()
                pos = stop
            }
            5 -> varint = fixed(4)
            else -> throw IllegalArgumentException("wire type $wire")
        }
        return true
    }

    fun string(): String = if (wire == 2) String(b, start, stop - start, Charsets.UTF_8) else ""

    fun message(): ProtoReader = if (wire == 2) ProtoReader(b, start, stop) else ProtoReader(b, 0, 0)

    private fun readVarint(): Long {
        var r = 0L
        var shift = 0
        while (true) {
            require(pos < end && shift < 64) { "bad varint" }
            val x = b[pos++].toInt()
            r = r or ((x and 0x7f).toLong() shl shift)
            if (x and 0x80 == 0) return r
            shift += 7
        }
    }

    private fun fixed(n: Int): Long {
        require(pos + n <= end) { "fixed$n past the end" }
        var r = 0L
        for (i in 0 until n) r = r or ((b[pos + i].toLong() and 0xff) shl (8 * i))
        pos += n
        return r
    }
}

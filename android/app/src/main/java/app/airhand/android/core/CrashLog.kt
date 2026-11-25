package app.airhand.android.core

import android.app.ActivityManager
import android.app.ActivityManager.RunningAppProcessInfo
import android.app.ApplicationExitInfo
import android.content.Context
import android.os.Build
import android.os.Process
import android.util.Log
import androidx.annotation.RequiresApi
import app.airhand.android.BuildConfig
import app.airhand.android.data.JsonFile
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import org.json.JSONObject
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.InputStream
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import kotlin.concurrent.thread
import kotlin.math.abs
import kotlin.system.exitProcess

/**
 * 崩溃日志：应用闪退、无响应、在前台时被系统杀掉，都记下来，用户在"设置 → 崩溃日志"里复制 / 分享给开发者
 * （贴到 issue 里，不用电脑和 adb）。
 *
 * * Java / Kotlin 的未捕获异常：[install] 设的处理器当场写下调用栈，再交给系统原来的处理器（照常退出）；
 * * 原生崩溃（.so 里的 SIGSEGV 等）、无响应（ANR）、在前台时被系统杀掉：进程已经没了，下次启动时从系统记的
 *   退出原因里取（[ApplicationExitInfo]，Android 11+；原生崩溃的调用栈要 Android 12+，见 [Tombstone]）。
 *   装新版覆盖安装之后，旧版本时的崩溃也取得到（只是没有 Java 的调用栈）；
 * * 只放在应用自己的数据目录里（`files/crashes/`，最多 [MAX_RECORDS] 条），不上传到任何地方，卸载时和别的数据一起删掉。
 *   内容是应用版本、机型、系统版本和出错的位置，没有画面和输入的内容。
 *
 * 记录是一个个文本文件，文件名 `<Unix 毫秒>-<种类>-<pid>.txt`；`state.json` 记着处理到哪里了（见 [State]）
 */
object CrashLog {
    /** 记了几条、最近一条的时间（Unix 毫秒，没有时为 0） */
    data class Summary(val count: Int = 0, val latest: Long = 0)

    private const val TAG = "airhand"
    private const val DIR = "crashes"
    private const val STATE = "state.json"
    const val MAX_RECORDS = 10

    /** 一条记录里 Java 调用栈最多这么多字符（栈溢出的调用栈能有上千层） */
    private const val MAX_TRACE = 32 * 1024

    /** 无响应时的线程转储最多这么多字节：开头就是主线程，后面的线程用处不大 */
    private const val MAX_ANR = 24 * 1024

    /** tombstone 最多读这么多：里面还有内存映射、打开的文件、日志，比要的大得多；要的部分在前面 */
    private const val MAX_TOMBSTONE = 4 * 1024 * 1024

    /** 导出的总长度：GitHub 一条评论最多 65536 个字符 */
    const val MAX_EXPORT = 60_000

    /** Java 崩溃和系统记的同一次退出：pid 相同、时间差在这之内的算一次 */
    private const val SAME_CRASH_MS = 60_000L

    private val lock = Any()

    @Volatile
    private var dir: File? = null

    private val _summary = MutableStateFlow(Summary())
    val summary: StateFlow<Summary> = _summary.asStateFlow()

    private val _unseen = MutableStateFlow(false)

    /** 有还没提示过用户的新记录（启动时检查完才知道，见 [markSeen]） */
    val unseen: StateFlow<Boolean> = _unseen.asStateFlow()

    /** 应用启动时最先调用：设好未捕获异常的处理器，在后台检查上次以来系统记的退出原因 */
    fun install(context: Context) {
        val d = File(context.filesDir, DIR)
        dir = d
        val previous = Thread.getDefaultUncaughtExceptionHandler()
        Thread.setDefaultUncaughtExceptionHandler { t, e ->
            try {
                writeJava(d, t, e)
            } catch (_: Throwable) {
                // 记不下来（磁盘满了、内存不够）也要照常退出
            }
            if (previous != null) {
                previous.uncaughtException(t, e)
            } else {
                Process.killProcess(Process.myPid())
                exitProcess(10)
            }
        }
        val app = context.applicationContext
        thread(name = "airhand-crashlog", isDaemon = true) { scan(app) }
    }

    /** 已经提示过用户了：这些记录不再提示 */
    fun markSeen() {
        synchronized(lock) {
            val d = dir ?: return
            State.load(File(d, STATE)).copy(notified = _summary.value.latest).save(d)
            _unseen.value = false
        }
    }

    /** 全部记录，新的在前，连起来不超过 [MAX_EXPORT] 个字符（太多时只要新的）。会读文件，别在主线程调用 */
    fun export(): String {
        synchronized(lock) {
            val d = dir ?: return ""
            val texts = records(d).mapNotNull { r -> runCatching { r.file.readText() }.getOrNull()?.takeIf { it.isNotBlank() } }
            return joinRecords(texts, MAX_EXPORT)
        }
    }

    /** 删掉全部记录。会删文件，别在主线程调用 */
    fun clear() {
        synchronized(lock) {
            val d = dir ?: return
            records(d).forEach { it.file.delete() }
            refresh(d, notified = Long.MAX_VALUE)
        }
    }

    // ───────────────────────────── 记录 ─────────────────────────────

    private class Record(val file: File, val at: Long, val kind: String, val pid: Int)

    private val NAME = Regex("""(\d{1,18})-([a-z]+)-(\d{1,9})\.txt""")

    /** 新的在前 */
    private fun records(d: File): List<Record> = d.listFiles().orEmpty().mapNotNull { f ->
        val m = NAME.matchEntire(f.name) ?: return@mapNotNull null
        Record(f, m.groupValues[1].toLong(), m.groupValues[2], m.groupValues[3].toInt())
    }.sortedByDescending { it.at }

    private fun write(d: File, at: Long, kind: String, pid: Int, text: String) {
        d.mkdirs()
        File(d, "$at-$kind-$pid.txt").writeText(text)
    }

    /** 在崩溃的线程上，进程马上就没了：只写文件，不碰锁和别的状态 */
    private fun writeJava(d: File, t: Thread, e: Throwable) {
        val now = System.currentTimeMillis()
        val text = buildString {
            header(this, "Java crash", now, currentVersion())
            appendLine("Thread: ${t.name}")
            append(e.stackTraceToString().let { if (it.length > MAX_TRACE) it.take(MAX_TRACE) + "\n\t…" else it })
        }
        write(d, now, KIND_JAVA, Process.myPid(), text)
    }

    private fun header(out: StringBuilder, kind: String, at: Long, version: String) {
        out.appendLine("==== $kind · ${time(at)} ====")
        out.appendLine("Airhand $version · ${device()}")
    }

    private fun currentVersion() = "${BuildConfig.VERSION_NAME} (${BuildConfig.VERSION_CODE})" + if (BuildConfig.DEBUG) " debug" else ""

    private fun device(): String {
        val abi = Build.SUPPORTED_ABIS.firstOrNull() ?: "?"
        val bits = if (Process.is64Bit()) "64-bit" else "32-bit"
        return "${Build.MANUFACTURER} ${Build.MODEL} · Android ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT}) · " +
            "${Build.DISPLAY} · $abi, $bits"
    }

    private fun time(ms: Long): String = SimpleDateFormat("yyyy-MM-dd HH:mm:ss.SSS Z", Locale.US).format(Date(ms))

    // ───────────────────────────── 启动时的检查 ─────────────────────────────

    /**
     * 处理到哪里了。`exitSeen`：看过的系统退出记录里最新的时间；`notified`：提示过用户的最新一条记录的时间；
     * `version`：上次运行的版本（系统记的退出发生在之前的运行里，用它标版本；没有的话是加上崩溃日志之前的版本）
     */
    private data class State(val exitSeen: Long = 0, val notified: Long = 0, val version: String? = null) {
        fun save(d: File) {
            d.mkdirs()
            val o = JSONObject().put("exitSeen", exitSeen).put("notified", notified).put("version", version ?: JSONObject.NULL)
            JsonFile.write(File(d, STATE), o.toString())
        }

        companion object {
            fun load(f: File): State = JsonFile.read(f)?.let {
                runCatching {
                    val o = JSONObject(it)
                    State(o.optLong("exitSeen"), o.optLong("notified"), o.optStringOrNull("version"))
                }.getOrNull()
            } ?: State()
        }
    }

    private fun scan(context: Context) {
        synchronized(lock) {
            val d = dir ?: return
            try {
                val state = State.load(File(d, STATE))
                var exitSeen = state.exitSeen
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                    exitSeen = try {
                        recordExits(context, d, state)
                    } catch (e: Exception) {
                        // 有的系统不让读，或者读到一半出错：下次再试
                        Log.w(TAG, "can't read the exit reasons: $e")
                        state.exitSeen
                    }
                }
                records(d).drop(MAX_RECORDS).forEach { it.file.delete() }
                state.copy(exitSeen = exitSeen, version = currentVersion()).save(d)
                refresh(d, state.notified)
            } catch (e: Exception) {
                Log.w(TAG, "crash log: $e")
            }
        }
    }

    private fun refresh(d: File, notified: Long) {
        val all = records(d)
        val latest = all.firstOrNull()?.at ?: 0
        _summary.value = Summary(all.size, latest)
        _unseen.value = latest > notified
    }

    /** 上次以来系统记的退出（只看要记的那些，见 [worthRecording]），返回看过的最新的时间 */
    @RequiresApi(Build.VERSION_CODES.R)
    private fun recordExits(context: Context, d: File, state: State): Long {
        val am = context.getSystemService(ActivityManager::class.java) ?: return state.exitSeen
        val infos = am.getHistoricalProcessExitReasons(context.packageName, 0, 16)
        val version = state.version ?: "(a version before ${BuildConfig.VERSION_NAME})"
        var seen = state.exitSeen
        for (info in infos.sortedBy { it.timestamp }) {
            if (info.timestamp <= state.exitSeen) continue
            seen = maxOf(seen, info.timestamp)
            if (!worthRecording(info.reason, info.importance)) continue
            // Java 崩溃当场记过了（带调用栈）：系统记的同一次退出不用再记
            if (info.reason == ApplicationExitInfo.REASON_CRASH && records(d).any {
                    it.kind == KIND_JAVA && it.pid == info.pid && abs(it.at - info.timestamp) < SAME_CRASH_MS
                }
            ) continue
            val text = try {
                describe(info, version)
            } catch (e: Exception) {
                buildString {
                    header(this, kindName(info.reason, info.status), info.timestamp, version)
                    appendLine("(couldn't read the details: $e)")
                }
            }
            write(d, info.timestamp, fileKind(info.reason), info.pid, text)
        }
        return seen
    }

    @RequiresApi(Build.VERSION_CODES.R)
    private fun describe(info: ApplicationExitInfo, version: String): String = buildString {
        header(this, kindName(info.reason, info.status), info.timestamp, version)
        append("Process: ${info.processName} · pid ${info.pid} · ${importanceName(info.importance)}")
        if (info.pss > 0) append(" · pss ${info.pss / 1024} MB")
        if (info.rss > 0) append(" · rss ${info.rss / 1024} MB")
        appendLine()
        info.description?.takeIf { it.isNotBlank() }?.let { appendLine("Description: $it") }
        when (info.reason) {
            ApplicationExitInfo.REASON_CRASH -> appendLine("(the stack trace wasn't recorded: this version didn't keep crash logs yet)")
            ApplicationExitInfo.REASON_CRASH_NATIVE -> {
                // Android 12 起是 tombstone（protobuf）；Android 11 没有
                val bytes = runCatching { info.traceInputStream?.use { it.readAtMost(MAX_TOMBSTONE) } }.getOrNull()
                when {
                    bytes == null -> appendLine("(no tombstone from the system)")
                    else -> appendLine(Tombstone.summary(bytes) ?: "(the tombstone couldn't be read, ${bytes.size} bytes)")
                }
            }
            ApplicationExitInfo.REASON_ANR -> {
                val bytes = runCatching { info.traceInputStream?.use { it.readAtMost(MAX_ANR + 1) } }.getOrNull()
                if (bytes == null) {
                    appendLine("(no thread dump from the system)")
                } else {
                    appendLine()
                    append(String(bytes, 0, minOf(bytes.size, MAX_ANR), Charsets.UTF_8).trimEnd())
                    appendLine(if (bytes.size > MAX_ANR) "\n…" else "")
                }
            }
            // 被系统杀掉的：系统不给调用栈，上面的原因、说明和内存就是全部
            else -> {}
        }
    }

    private fun InputStream.readAtMost(max: Int): ByteArray {
        val out = ByteArrayOutputStream()
        val buf = ByteArray(8192)
        while (out.size() < max) {
            val n = read(buf, 0, minOf(buf.size, max - out.size()))
            if (n < 0) break
            out.write(buf, 0, n)
        }
        return out.toByteArray()
    }

    private const val KIND_JAVA = "java"

    /**
     * 要记的退出：崩溃、无响应、启动失败、被系统因为占用太多资源杀掉总要记；内存不够、被信号杀掉只在用户正看着
     * （前台、前台服务、可见）时才算"闪退"——在后台被回收是正常的
     */
    @RequiresApi(Build.VERSION_CODES.R)
    internal fun worthRecording(reason: Int, importance: Int): Boolean = when (reason) {
        ApplicationExitInfo.REASON_CRASH,
        ApplicationExitInfo.REASON_CRASH_NATIVE,
        ApplicationExitInfo.REASON_ANR,
        ApplicationExitInfo.REASON_INITIALIZATION_FAILURE,
        ApplicationExitInfo.REASON_EXCESSIVE_RESOURCE_USAGE,
        ApplicationExitInfo.REASON_DEPENDENCY_DIED,
        -> true
        ApplicationExitInfo.REASON_LOW_MEMORY,
        ApplicationExitInfo.REASON_SIGNALED,
        -> importance <= RunningAppProcessInfo.IMPORTANCE_VISIBLE
        else -> false
    }

    @RequiresApi(Build.VERSION_CODES.R)
    private fun kindName(reason: Int, status: Int): String = when (reason) {
        ApplicationExitInfo.REASON_CRASH -> "Java crash"
        ApplicationExitInfo.REASON_CRASH_NATIVE -> "Native crash"
        ApplicationExitInfo.REASON_ANR -> "Not responding (ANR)"
        ApplicationExitInfo.REASON_INITIALIZATION_FAILURE -> "Failed to start"
        ApplicationExitInfo.REASON_EXCESSIVE_RESOURCE_USAGE -> "Killed by the system: excessive resource use"
        ApplicationExitInfo.REASON_DEPENDENCY_DIED -> "Killed by the system: a dependency died"
        ApplicationExitInfo.REASON_LOW_MEMORY -> "Killed by the system: low memory"
        ApplicationExitInfo.REASON_SIGNALED -> "Killed by signal $status"
        else -> "Exited (reason $reason)"
    }

    @RequiresApi(Build.VERSION_CODES.R)
    private fun fileKind(reason: Int): String = when (reason) {
        ApplicationExitInfo.REASON_CRASH -> "crash"
        ApplicationExitInfo.REASON_CRASH_NATIVE -> "native"
        ApplicationExitInfo.REASON_ANR -> "anr"
        else -> "exit"
    }

    private fun importanceName(importance: Int): String = when {
        importance <= RunningAppProcessInfo.IMPORTANCE_FOREGROUND -> "foreground"
        importance <= RunningAppProcessInfo.IMPORTANCE_FOREGROUND_SERVICE -> "foreground service"
        importance <= RunningAppProcessInfo.IMPORTANCE_VISIBLE -> "visible"
        else -> "background"
    } + " ($importance)"

    /**
     * 把几条记录（新的在前）连起来，不超过 `max` 个字符：放不下的旧记录不要，最后说一声少了几条；
     * 最新的一条自己就放不下时截掉它的后面
     */
    internal fun joinRecords(records: List<String>, max: Int): String {
        val reserve = 64
        val limit = (max - reserve).coerceAtLeast(1)
        val out = StringBuilder()
        var used = 0
        for (r in records) {
            val text = r.trimEnd()
            val sep = if (out.isEmpty()) "" else "\n\n"
            if (out.length + sep.length + text.length > limit) {
                if (out.isEmpty()) {
                    out.append(text, 0, limit - 1).append('…')
                    used++
                }
                break
            }
            out.append(sep).append(text)
            used++
        }
        val left = records.size - used
        if (left > 0) out.append("\n\n(").append(left).append(if (left == 1) " older record" else " older records").append(" left out)")
        return out.toString()
    }
}

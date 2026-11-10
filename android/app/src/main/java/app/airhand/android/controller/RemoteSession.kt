package app.airhand.android.controller

import android.graphics.Bitmap
import android.graphics.Color
import android.os.SystemClock
import android.util.Log
import android.view.Surface
import app.airhand.android.AirhandApp
import app.airhand.android.core.Codec
import app.airhand.android.core.DisplayInfo
import app.airhand.android.core.EncoderInfo
import app.airhand.android.core.HostHello
import app.airhand.android.core.Native
import app.airhand.android.core.Notice
import app.airhand.android.core.OsKind
import app.airhand.android.core.Preset
import app.airhand.android.core.Problem
import app.airhand.android.core.Quality
import app.airhand.android.core.ServerState
import app.airhand.android.core.Stats
import app.airhand.android.core.SystemAction
import app.airhand.android.core.optStringOrNull
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import org.json.JSONArray
import org.json.JSONObject
import kotlin.concurrent.thread

/**
 * 远端光标：形状由被控端发来，位置由本机决定（触控板模式下本机画出来）。
 * 隐藏了（游戏里转视角等）时鼠标改发相对移动（见 [Input.delta]）
 */
data class CursorState(val visible: Boolean = true, val bitmap: Bitmap? = null, val hotX: Int = 0, val hotY: Int = 0)

/**
 * 一个远程会话（看 / 操作一台设备）。连接、断线重连、等对方上线都在这里，界面只观察 [phase] 等状态。
 *
 * * 对方要先问它的用户（手机默认这样）：[Phase.Approving]，同意之后才是 [Phase.Live]；
 * * 第一次连接失败（控制码不对、对方不在线、对方拒绝……）：[Phase.Failed]，回到首页显示原因；
 * * 连上之后断了：带着重连凭证自动重连（[Phase.Reconnecting]）；对方离线了就等它上线（[Phase.WaitingHost]）；
 * * 对方断开了我们 / 换了控制码：[Phase.Ended]，不再重连。
 *
 * 对方同意、开始会话时把设备（和重连凭证）记进"我的设备"，下次点一下就能连；对方不在线时也能等它上线。
 */
class RemoteSession(
    private val app: AirhandApp,
    val code: String,
    resume: String?,
    /** 只看画面（电视）：不发送任何输入 */
    val viewOnly: Boolean,
    /** 之前记下的对方公钥（我的设备）：只认它，对不上是 hostKeyChanged。null：第一次连 */
    hostKey: String? = null,
    /** 配对码（对方要求配对、或者对方的身份变了要重新配对时用户输入的），只用于这一次连接 */
    pairing: String? = null,
) {
    sealed interface Phase {
        data object Connecting : Phase

        /** 连上了，在等对方的用户同意 */
        data object Approving : Phase
        data object Live : Phase
        data class Reconnecting(val reason: Problem) : Phase
        data class WaitingHost(val reason: Problem?) : Phase
        data class Failed(val reason: Problem) : Phase
        data class Ended(val reason: Problem) : Phase
        data object Closed : Phase
    }

    private val client = app.controller
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)

    private val _phase = MutableStateFlow<Phase>(Phase.Connecting)
    val phase: StateFlow<Phase> = _phase.asStateFlow()

    private val saved = app.devices.get(code)
    val hostName = MutableStateFlow(saved?.title() ?: "")
    val hostOs = MutableStateFlow(saved?.hostOs ?: "")
    val hello = MutableStateFlow<HostHello?>(null)
    val displays = MutableStateFlow<List<DisplayInfo>>(emptyList())
    val currentDisplay = MutableStateFlow(0)
    val encoder = MutableStateFlow<EncoderInfo?>(null)
    val stats = MutableStateFlow<Stats?>(null)

    /** 被控端的提示，按类别（capture / audio / permissions / other）各保留一条 */
    val notices = MutableStateFlow<Map<String, Notice>>(emptyMap())
    val videoSize = MutableStateFlow<Pair<Int, Int>?>(null)
    val cursor = MutableStateFlow(CursorState())
    val audioOn = MutableStateFlow(app.settings.value.audio)
    val avSyncOn = MutableStateFlow(app.settings.value.avSync)

    /** 锁定鼠标：鼠标一直只发移动了多少、不发位置（被控端隐藏光标时会自动这样，这是手动打开的） */
    val mouseLock = MutableStateFlow(false)

    /** 本机播放声音出错 */
    val audioError = MutableStateFlow<String?>(null)
    val quality = MutableStateFlow(Preset.of(app.settings.value.preset).quality(VideoDecoder.decodable.firstOrNull { it in Codec.PRIORITY } ?: Codec.H264))

    private val _clipboard = MutableSharedFlow<String>(extraBufferCapacity = 4)

    /** 被控端复制了文本（已经写进本机剪贴板），界面提示一下 */
    val clipboardIn: SharedFlow<String> = _clipboard.asSharedFlow()

    /** 对方的平台：连上之后以对方报告的为准，之前按系统名称猜 */
    val os: OsKind get() = hello.value?.platform?.takeIf { it != OsKind.UNKNOWN } ?: OsKind.of(hostOs.value)

    @Volatile
    private var resume: String? = resume

    /** 对方的端到端加密公钥：连上时确认过的（之后重连只认它） */
    @Volatile
    private var hostKey: String? = hostKey

    /** 配对码：连上之后就不要了 */
    @Volatile
    private var pairing: String? = pairing

    /** 安全码（连上之后才有）：对方屏幕上显示的是同一个，不一样就是中间有人 */
    val safety = MutableStateFlow<String?>(null)

    @Volatile
    private var handle = 0L

    @Volatile
    private var sid = -1

    @Volatile
    private var closed = false

    @Volatile
    private var background = false
    private var connectedOnce = false

    /** 被控端按槽位缓存的光标形状（动画光标反复用到的那几帧只传一次，见 [Media]）。只在一个会话里有效，只在会话线程上用 */
    private val cursorSlots = HashMap<Int, CursorState>()
    private val wake = Object()
    private var woken = false

    private val sync = AvSync()
    private val decoder = VideoDecoder(
        sync,
        requestKeyframe = { command { Commands.keyframe(it) } },
        onTooLarge = { c, w, h -> onTooLarge(c, w, h) },
    )

    /** 收到过本机解码器放不下的画面：按画面尺寸算出的短边上限（只对那种格式有效；对方换了屏幕 / 转了屏就作废） */
    @Volatile
    private var frameLimit: Pair<Codec, Int>? = null

    /** 最近发给对方的画质设置 */
    @Volatile
    private var sentSettings: Quality? = null

    /** 这次连接在握手里让对方发声音了没有 */
    @Volatile
    private var sentAudio = false
    @Volatile private var audio: AudioPlayer? = null
    private var audioToken = 0L
    private val sender = InputSender { events ->
        if (!viewOnly) command { Commands.input(it, events) }
    }

    fun start() {
        // 等的被控端上线了 / 与服务器重新连上了：叫醒等待中的连接线程
        scope.launch {
            client.hostOnline.collect { if (it == code) signal() }
        }
        scope.launch {
            var was: ServerState? = null
            client.server.collect {
                if (it is ServerState.Connected && was !is ServerState.Connected) signal()
                was = it
            }
        }
        thread(name = "airhand-session") { run() }
    }

    // ───────────────────────────── 连接 ─────────────────────────────

    private fun signal() = synchronized(wake) {
        woken = true
        wake.notifyAll()
    }

    /** 最多等 `ms`，被 [signal] 叫醒时提前返回 */
    private fun sleep(ms: Long) = synchronized(wake) {
        if (!woken && !closed) wake.wait(ms)
        woken = false
    }

    private fun run() {
        client.acquire()
        try {
            var attempts = 0
            while (!closed) {
                val serverProblem = waitServer(if (connectedOnce) Long.MAX_VALUE else FIRST_SERVER_WAIT_MS)
                if (closed) break
                if (serverProblem != null) {
                    _phase.value = Phase.Failed(serverProblem)
                    break
                }
                val h = client.handle()
                val settings = currentSettings()
                sentSettings = settings
                // 开始时的画质和要不要声音在端到端握手里就告诉对方：它一开始就按这个建编码器
                val withAudio = audioOn.value && !background
                sentAudio = withAudio
                val cmd = Commands.connect(code, resume, settings, hostKey, pairing, withAudio)
                val r = runCatching { JSONObject(Native.ctlCall(h, cmd.toString())) }
                    .getOrElse { JSONObject().put("err", JSONObject().put("kind", "failed").put("detail", it.toString())) }
                if (closed) {
                    r.optJSONObject("ok")?.let { Native.ctlCall(h, Commands.disconnect(it.getInt("sid")).toString()) }
                    break
                }
                val ok = r.optJSONObject("ok")
                if (ok != null) {
                    attempts = 0
                    onConnected(h, ok)
                    val end = pump(h, ok.getInt("sid")) ?: break
                    sid = -1
                    decoder.resetStream()
                    stopAudio()
                    if (closed) break
                    if (!end.second) {
                        // 对方拒绝 / 没回应 / 断开了我们：第一次就没进去的话回到首页说明原因
                        _phase.value = if (connectedOnce) Phase.Ended(end.first) else Phase.Failed(end.first)
                        break
                    }
                    _phase.value = Phase.Reconnecting(end.first)
                    sleep(RETRY_MS)
                    continue
                }
                val p = Problem.parse(r.optJSONObject("err"))
                attempts++
                when (p.kind) {
                    // 带着凭证（我的设备里保存的、或者断线重连）而对方不在线：等它。凭证作废了的话服务器回答的是
                    // kicked（见下面），所以不用再拿控制码试一次——那样会算一次猜错，占掉这个 IP 的额度
                    "hostOffline" -> {
                        _phase.value = Phase.WaitingHost(p)
                        // 对方上线时服务器通知（signal）；保险起见隔一会儿也再问一次
                        sleep(OFFLINE_RECHECK_MS)
                    }
                    // 重连凭证作废了（对方断开过所有控制端，或者换了控制码）：还没连上过（从我的设备打开）就丢掉凭证、
                    // 用控制码直接连；连上过就不再自动重连
                    "kicked" -> {
                        val retryPlain = resume != null && !connectedOnce
                        resume = null
                        if (retryPlain) continue
                        _phase.value = if (connectedOnce) Phase.Ended(p) else Phase.Failed(p)
                        break
                    }
                    "notConnected", "serverTimeout", "serverLost" -> {
                        if (!connectedOnce && attempts >= 3) {
                            _phase.value = Phase.Failed(p)
                            break
                        }
                        if (connectedOnce) _phase.value = Phase.Reconnecting(p)
                        sleep(RETRY_MS * 2)
                    }
                    else -> {
                        _phase.value = if (connectedOnce) Phase.Ended(p) else Phase.Failed(p)
                        break
                    }
                }
            }
        } catch (e: Exception) {
            Log.e(TAG, "session thread failed", e)
            _phase.value = Phase.Failed(Problem("failed", e.toString()))
        } finally {
            stopAudio()
            client.release()
            if (closed) _phase.value = Phase.Closed
        }
    }

    /** 等到连上服务器。返回 null 表示连上了；第一次连接时等不到就返回原因 */
    private fun waitServer(limitMs: Long): Problem? {
        val start = SystemClock.elapsedRealtime()
        while (!closed) {
            when (val s = client.server.value) {
                is ServerState.Connected -> if (client.handle() != 0L) return null
                is ServerState.Disconnected -> {
                    // 版本不一致、证书不对、设置写错了：等下去也不会好
                    if (!connectedOnce && s.reason.kind in listOf("incompatible", "certMismatch", "failed")) return s.reason
                    if (SystemClock.elapsedRealtime() - start > limitMs) return s.reason
                }
                else -> if (SystemClock.elapsedRealtime() - start > limitMs) return Problem("serverTimeout")
            }
            Thread.sleep(100)
        }
        return null
    }

    /** 服务器给出的对方名称和系统，开始会话时记进"我的设备" */
    private var grantName = ""
    private var grantOs = ""

    /** 连上了被控端（还没开始：对方可能要先问它的用户，见 [onHello]） */
    private fun onConnected(h: Long, ok: JSONObject) {
        handle = h
        sid = ok.getInt("sid")
        // 光标的槽位是被控端为一个会话分配的：新的会话从头开始
        cursorSlots.clear()
        resume = Resume.fromJson(ok.optJSONArray("resume")) ?: resume
        // 端到端握手确认过的对方公钥：之后重连只认它；配对码用过了
        ok.optString("hostKey").takeIf { it.isNotBlank() }?.let { hostKey = it }
        safety.value = ok.optString("safety").takeIf { it.isNotBlank() }
        pairing = null
        grantName = ok.optString("hostName")
        grantOs = ok.optString("hostOs")
        if (grantName.isNotBlank() && saved?.alias.isNullOrBlank()) hostName.value = grantName
        hostOs.value = grantOs
        // 开始时要不要声音已经在握手里告诉对方了；连接期间变了（例如切到了后台）再告诉它（对方同意之前只是记下来）
        if (audioOn.value && !background) startAudio(tell = !sentAudio) else if (sentAudio) command { Commands.audio(it, false) }
        if (background) applyBackground()
        Log.i(TAG, "connected to $code ($grantName, $grantOs)")
    }

    /** 会话开始（对方同意了，或者不需要同意） */
    private fun onHello() {
        app.devices.connected(code, grantName, grantOs, resume ?: "", hostKey)
        connectedOnce = true
        _phase.value = Phase.Live
    }

    /** 读会话数据直到会话结束。返回 (原因, 是否重连)；用户关闭时返回 null */
    private fun pump(h: Long, sid: Int): Pair<Problem, Boolean>? {
        var quickNulls = 0
        while (!closed) {
            if (client.handle() != h) return Problem("serverLost", "connection restarted") to true
            val t0 = SystemClock.elapsedRealtime()
            val b = Native.ctlPollSession(h, sid, 250)
            if (b == null) {
                // 队列不在了（不应该发生）：别空转
                if (SystemClock.elapsedRealtime() - t0 < 50 && ++quickNulls > 20) return Problem("sessionEnded") to true
                continue
            }
            quickNulls = 0
            if (b.isEmpty()) continue
            if (b[0] == '{'.code.toByte()) {
                val e = runCatching { JSONObject(String(b, Charsets.UTF_8)) }.getOrNull() ?: continue
                if (e.optString("type") == "closed") {
                    Native.ctlCall(h, Commands.disconnect(sid).toString())
                    return Problem.parse(e.optJSONObject("reason")) to e.optBoolean("retry", true)
                }
                handleEvent(e)
            } else {
                handlePacket(h, sid, b)
            }
        }
        return null
    }

    /** 媒体包（格式见 [Media]） */
    private fun handlePacket(h: Long, sid: Int, b: ByteArray) {
        when (Media.kind(b)) {
            Media.VIDEO -> {
                val v = Media.video(b) ?: run {
                    // 不认识的编码格式：丢掉，但照样确认（否则对方的在途帧满了就不再发画面）
                    Native.ctlAck(h, sid, Media.seq(b))
                    return
                }
                if (videoSize.value != v.width to v.height) videoSize.value = v.width to v.height
                decoder.submit(
                    VideoPacket(
                        seq = v.seq,
                        key = v.key,
                        codec = v.codec,
                        width = v.width,
                        height = v.height,
                        ptsUs = v.captureUs,
                        data = b,
                        // 负载从头里写的长度处开始（以后的版本可能把头加长）
                        offset = Media.payload(b),
                        length = b.size - Media.payload(b),
                        ack = { Native.ctlAck(h, sid, v.seq) },
                    ),
                )
            }
            Media.CURSOR -> {
                val c = Media.cursor(b)
                val s = c.shape
                // 新的形状存进槽位；只带槽位的（例如动画光标的下一帧）用槽位里已有的，万一没有就保持原样
                val shape = when {
                    s != null -> cursorBitmap(b, s.width, s.height)
                        ?.let { CursorState(c.visible, it, s.hotX, s.hotY) }
                        ?.also { cursorSlots[c.slot] = it }
                    c.cached -> cursorSlots[c.slot]
                    else -> null
                }
                cursor.value = (shape ?: cursor.value).copy(visible = c.visible)
            }
            Media.AUDIO -> {
                val a = Media.audio(b)
                val o = Media.payload(b)
                audio?.submit(AudioPacket(a.start, a.captureUs, b, o, b.size - o))
            }
        }
    }

    private fun handleEvent(e: JSONObject) {
        when (e.optString("type")) {
            "hello" -> {
                val codecs = e.optJSONArray("codecs").strings().mapNotNull { Codec.fromWire(it) }
                val h = HostHello(
                    hostname = e.optString("hostname"),
                    os = e.optString("os"),
                    version = e.optString("version"),
                    displays = DisplayInfo.parseList(e.optJSONArray("displays")),
                    current = e.optInt("current"),
                    codecs = codecs,
                    platform = OsKind.fromWire(e.optString("platform")),
                    touch = e.optBoolean("touch"),
                    actions = e.optJSONArray("actions").strings().mapNotNull { SystemAction.fromWire(it) },
                )
                hello.value = h
                onHello()
                displays.value = h.displays
                currentDisplay.value = h.current
                if (h.hostname.isNotBlank() && saved?.alias.isNullOrBlank()) hostName.value = h.hostname
                // 用户选的格式；自动时选双方都支持的压缩率最高的格式（对方能编码、本机能硬件解码）
                val best = pickCodec(codecs)
                if (quality.value.codec != best) setQuality(quality.value.copy(codec = best)) else refreshSettings()
            }
            "awaitingApproval" -> _phase.value = Phase.Approving
            "encoder" -> encoder.value = EncoderInfo(
                Codec.fromWire(e.optString("codec")) ?: Codec.H264,
                e.optString("name"),
                e.optBoolean("hardware"),
                e.optInt("width"),
                e.optInt("height"),
            )
            "displays" -> {
                displays.value = DisplayInfo.parseList(e.optJSONArray("displays"))
                currentDisplay.value = e.optInt("current")
                // 转了屏 / 换了屏幕：按新的尺寸重新算解码器放不放得下
                frameLimit = null
                refreshSettings()
            }
            "stats" -> stats.value = Stats(
                e.optDouble("fps").toFloat(),
                e.optDouble("kbps").toFloat(),
                if (e.isNull("rttMs")) null else e.optDouble("rttMs").toFloat(),
                e.optInt("width"),
                e.optInt("height"),
            )
            "notice" -> {
                val o = e.optJSONObject("notice") ?: return
                val n = Notice(o.optString("kind"), o.optStringOrNull("detail"), o.optBoolean("screen"), o.optBoolean("input"))
                val m = notices.value.toMutableMap()
                when (n.kind) {
                    "captureRecovered" -> m.remove("capture")
                    "audioRecovered" -> m.remove("audio")
                    "captureFailed" -> m["capture"] = n
                    "audioFailed" -> m["audio"] = n
                    "missingPermissions" -> if (!n.screen && !n.input) m.remove("permissions") else m["permissions"] = n
                    else -> m[n.kind] = n
                }
                notices.value = m
            }
            "clipboard" -> {
                val text = e.optString("text")
                if (text.isNotEmpty()) {
                    scope.launch {
                        app.clipboard.write(text)
                        _clipboard.tryEmit(text)
                    }
                }
            }
        }
    }

    private fun currentSettings(): Quality {
        val q = quality.value.let { it.copy(maxShort = decodableMaxShort(it)) }
        return if (background) q.copy(fps = BACKGROUND_FPS) else q
    }

    /**
     * 本机解码器放得下的短边上限。电视的硬件解码器常常只到 1080p，或者不认竖着的尺寸，
     * 竖屏手机原样的 1080×2400 发过来就一直黑屏：按对方屏幕的尺寸（和收到过的放不下的画面）缩小一些。
     * 界面上的画质选择不变。
     */
    private fun decodableMaxShort(q: Quality): Int {
        var s = q.maxShort
        displays.value.firstOrNull { it.index == currentDisplay.value }?.let {
            s = VideoDecoder.fitMaxShort(q.codec, it.width, it.height, s)
        }
        frameLimit?.let { (c, limit) -> if (c == q.codec) s = VideoFit.stricter(s, limit) }
        return s
    }

    /** 画面太大、本机解码器放不下（对方报的屏幕尺寸可能是逻辑尺寸，比实际画面小）：请对方缩小 */
    private fun onTooLarge(c: Codec, w: Int, h: Int) {
        val limit = VideoDecoder.fitMaxShort(c, w, h, 0)
        if (limit == 0) return
        val prev = frameLimit?.takeIf { it.first == c }?.second ?: 0
        frameLimit = c to VideoFit.stricter(prev, limit)
        Log.i(TAG, "${w}x$h doesn't fit the ${c.label} decoder, asking for a short side of at most ${frameLimit?.second}")
        refreshSettings()
    }

    /** 实际要发的设置变了（对方屏幕变了、解码器放不下）就发给对方 */
    private fun refreshSettings() {
        if (currentSettings() != sentSettings) sendSettings()
    }

    private fun sendSettings() {
        val s = currentSettings()
        sentSettings = s
        command { Commands.settings(it, s) }
    }

    // ───────────────────────────── 命令 ─────────────────────────────

    /** 给当前会话发命令（[Commands]，`make` 拿到会话的 sid）。没连上时丢掉 */
    private fun command(make: (sid: Int) -> JSONObject) {
        val h = handle
        val s = sid
        if (h == 0L || s < 0 || closed) return
        val cmd = make(s)
        val r = Native.ctlCall(h, cmd.toString())
        if (r.contains("\"err\"")) Log.w(TAG, "${cmd.optString("cmd")} failed: $r")
    }

    fun setSurface(surface: Surface?) = decoder.setSurface(surface)

    /** 输入事件（触控、键盘）。`now=false` 的会和后面的鼠标移动合并 */
    fun input(events: List<Any>, now: Boolean = true) {
        if (!viewOnly && _phase.value == Phase.Live) sender.send(events, now)
    }

    fun setQuality(q: Quality) {
        quality.value = q
        sendSettings()
    }

    /** 双方都支持的编码格式（对方能编码、本机能硬件解码），压缩率从高到低；还没连上时为空 */
    fun usableCodecs(): List<Codec> = usableCodecs(hello.value?.codecs ?: emptyList())

    private fun usableCodecs(host: List<Codec>): List<Codec> = Codec.PRIORITY.filter { it in host && it in VideoDecoder.decodable }

    /** 要用的格式：用户在设置里选的（双方都支持时），否则双方都支持的压缩率最高的 */
    private fun pickCodec(host: List<Codec>): Codec {
        val usable = usableCodecs(host)
        val preferred = Codec.fromWire(app.settings.value.codec)
        return preferred?.takeIf { it in usable } ?: usable.firstOrNull() ?: Codec.H264
    }

    /** 选编码格式（null = 自动）：记进设置，以后的会话（包括重连）也用它 */
    fun setCodec(c: Codec?) {
        app.updateSettings { it.copy(codec = c?.wire ?: "") }
        val host = hello.value?.codecs ?: return
        val next = pickCodec(host)
        if (quality.value.codec != next) setQuality(quality.value.copy(codec = next))
    }

    fun selectDisplay(index: Int) {
        command { Commands.display(it, index) }
    }

    fun setAudio(on: Boolean) {
        audioOn.value = on
        if (on) startAudio() else stopAudio()
    }

    @Synchronized
    fun setAvSync(on: Boolean) {
        avSyncOn.value = on
        app.updateSettings { it.copy(avSync = on) }
        sync.setEnabled(on && audio != null)
        decoder.refreshTiming()
    }

    /** `tell`：告诉对方开始发声音（连上的时候对方已经按握手里说的开始了，不用） */
    @Synchronized
    private fun startAudio(tell: Boolean = true) {
        if (closed || background || !audioOn.value) return
        if (audio == null) {
            audioError.value = null
            val token = sync.reset()
            audioToken = token
            sync.setEnabled(avSyncOn.value)
            audio = AudioPlayer(sync, token) { error ->
                synchronized(this) {
                    if (audioToken == token) {
                        audioError.value = error
                        audio = null
                        audioToken = sync.reset()
                        sync.setEnabled(false)
                        decoder.refreshTiming()
                    }
                }
            }
        }
        if (tell) command { Commands.audio(it, true) }
    }

    @Synchronized
    private fun stopAudio() {
        command { Commands.audio(it, false) }
        audio?.release()
        audio = null
        audioToken = sync.reset()
        sync.setEnabled(false)
        decoder.refreshTiming()
    }

    /** 把文本同步到对方的剪贴板（对方之后可以粘贴） */
    /** 系统操作（只有对方在 Hello 里列出的才发） */
    fun system(action: SystemAction) {
        if (hello.value?.actions?.contains(action) == true) input(listOf(Input.system(action)))
    }

    fun sendClipboard(text: String) {
        if (text.isNotEmpty()) command { Commands.clipboard(it, text) }
    }

    /** 以键盘输入的方式把文本打到对方当前的输入框里 */
    fun typeText(text: String) {
        text.chunked(512).forEach { input(listOf(Input.text(it))) }
    }

    /** 立即重试（等对方上线时用户点了"重试"） */
    fun retryNow() = signal()

    /** 画面不在屏幕上（应用切到后台）：降到很低的帧率，回来时恢复并要一个关键帧 */
    fun setBackground(bg: Boolean) {
        if (background == bg) return
        background = bg
        applyBackground()
    }

    private fun applyBackground() {
        sendSettings()
        if (background) stopAudio() else if (audioOn.value) startAudio()
    }

    fun close() {
        if (closed) return
        closed = true
        signal()
        val (h, s) = handle to sid
        sender.release()
        if (h != 0L && s >= 0) {
            thread(name = "airhand-close") {
                Native.ctlCall(h, Commands.disconnect(s).toString())
            }
        }
        stopAudio()
        decoder.release()
        scope.cancel()
        if (_phase.value !is Phase.Failed) _phase.value = Phase.Closed
    }

    companion object {
        private const val TAG = "airhand"
        private const val FIRST_SERVER_WAIT_MS = 12_000L
        private const val RETRY_MS = 800L
        private const val OFFLINE_RECHECK_MS = 60_000L
        private const val BACKGROUND_FPS = 2

        /** 光标形状：头后面的 RGBA（非预乘）→ Bitmap */
        fun cursorBitmap(b: ByteArray, w: Int, h: Int): Bitmap? {
            var o = Media.payload(b)
            if (w == 0 || h == 0 || b.size < o + w * h * 4) return null
            val px = IntArray(w * h)
            for (i in px.indices) {
                px[i] = Color.argb(b[o + 3].toInt() and 0xff, b[o].toInt() and 0xff, b[o + 1].toInt() and 0xff, b[o + 2].toInt() and 0xff)
                o += 4
            }
            return Bitmap.createBitmap(px, w, h, Bitmap.Config.ARGB_8888)
        }
    }
}

private fun JSONArray?.strings(): List<String> = if (this == null) emptyList() else (0 until length()).map { optString(it) }

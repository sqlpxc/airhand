package app.airhand.android.ui.session

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import app.airhand.android.data.GameControl
import app.airhand.android.data.GameLayout
import kotlin.math.abs
import kotlin.math.hypot

/**
 * 编辑一个方案（纯逻辑，方便测试）：拖动按钮摆放，点一下选中（界面上出现它的设置），点空白处取消选中。
 *
 * 改的是一份副本（[layout]），点"完成"才保存，"取消"就丢掉。每一步都能撤销（拖动一次算一步；
 * 拖滑块这种连续的改动先 [snapshot] 一次，之后 `record = false`，整段算一步）。
 *
 * 拖动时按钮的中心靠近别的按钮的中心线、或者区域的中线时吸过去，画出一条辅助线（[guideX] / [guideY]），
 * 方便排整齐；按钮始终留在区域里。两个手指捏合调选中的按钮的大小（中心不动），也算一步撤销。
 *
 * [layout]、[selected]、[settings]、[canUndo] 是 Compose 的状态，界面直接读
 */
class LayoutEditor(
    val original: GameLayout,
    private val geo: GameGeometry,
    private val slop: Float,
    /** 要重画（按钮动了、选中变了） */
    private val redraw: () -> Unit,
    /** 吸附到辅助线上时轻轻振一下 */
    private val tick: () -> Unit = {},
) {
    var layout by mutableStateOf(original)
        private set

    /** 选中的按钮（界面显示它的设置） */
    var selected by mutableStateOf<Int?>(null)

    /** 显示方案设置（不透明度、灵敏度……） */
    var settings by mutableStateOf(false)

    var canUndo by mutableStateOf(false)
        private set

    /** 和打开时不一样了（取消时要确认） */
    val dirty: Boolean get() = layout != original

    /** 正在拖动的按钮 */
    var dragging: Int? = null
        private set

    /** 辅助线（像素），没有吸附时为 null */
    var guideX: Float? = null
        private set
    var guideY: Float? = null
        private set

    private val history = ArrayList<GameLayout>()

    private fun push(l: GameLayout) {
        history += l
        if (history.size > MAX_UNDO) history.removeAt(0)
        canUndo = true
    }

    /** 连续的改动（拖滑块）开始前记一步，之后的 [change] 用 `record = false` */
    fun snapshot() = push(layout)

    fun change(record: Boolean = true, f: (GameLayout) -> GameLayout) {
        val next = f(layout)
        if (next == layout) return
        if (record) push(layout)
        layout = next
        redraw()
    }

    /** 改一个按钮（改完拉回区域里：变大了可能会出界） */
    fun update(id: Int, record: Boolean = true, f: (GameControl) -> GameControl) = change(record) { l ->
        l.control(id)?.let { l.with(geo.inside(f(it))) } ?: l
    }

    fun undo() {
        if (history.isEmpty()) return
        layout = history.removeAt(history.lastIndex)
        canUndo = history.isNotEmpty()
        if (selected?.let(layout::control) == null) selected = null
        redraw()
    }

    /** 加一个按钮：放在画面中间附近的空地方，并选中它（马上能看到它的设置） */
    fun add(make: (id: Int) -> GameControl) {
        val c = make(layout.nextId())
        val (x, y) = freeSpot(c.size, geo.left + geo.width * 0.5f, geo.top + geo.height * 0.45f)
        val placed = c.at(x, y)
        change { it.plus(placed) }
        selected = placed.id
        settings = false
    }

    /** 复制一个按钮，放在它旁边的空地方 */
    fun duplicate(id: Int) {
        val c = layout.control(id) ?: return
        val copy = c.withId(layout.nextId())
        val (x, y) = freeSpot(c.size, geo.cx(c) + geo.radius(c) * 2.4f, geo.cy(c))
        val placed = copy.at(x, y)
        change { it.plus(placed) }
        selected = placed.id
    }

    fun delete(id: Int) {
        change { it.without(id) }
        if (selected == id) selected = null
    }

    /**
     * 离 (`nearX`, `nearY`)（像素）最近、不和别的按钮挤在一起的位置（比例）。
     * 实在没有空地方就放在那里（拉回区域里）
     */
    fun freeSpot(size: Float, nearX: Float, nearY: Float): Pair<Float, Float> {
        if (!geo.ready) return 0.5f to 0.5f
        val r = geo.radius(size)
        val gap = 6 * geo.density
        val step = maxOf(r * 0.5f, 4 * geo.density)
        var best: Pair<Float, Float>? = null
        var bestD = Float.MAX_VALUE
        var y = geo.top + r
        while (y <= geo.top + geo.height - r) {
            var x = geo.left + r
            while (x <= geo.left + geo.width - r) {
                val d = hypot(x - nearX, y - nearY)
                if (d < bestD && layout.controls.all { hypot(geo.cx(it) - x, geo.cy(it) - y) >= geo.radius(it) + r + gap }) {
                    best = x to y
                    bestD = d
                }
                x += step
            }
            y += step
        }
        val (px, py) = best ?: (nearX to nearY)
        return geo.place(size, px, py)
    }

    // ───────────── 拖动、捏合 ─────────────

    /** 按着的手指和位置 */
    private val fingers = LinkedHashMap<Int, Pair<Float, Float>>()

    /** 捏合调大小：哪个按钮、开始时多大、两指开始时相距多远、开始前的方案（撤销用） */
    private class Pinch(val id: Int, val size: Float, val dist: Float, val before: GameLayout)

    private var pinch: Pinch? = null

    /** 拖动按钮的手指（第一个手指） */
    private var pointer: Int? = null
    private var startX = 0f
    private var startY = 0f
    private var grabX = 0f
    private var grabY = 0f
    private var moved = false
    private var before: GameLayout? = null

    /** 第一个手指拖动、点选；第二个手指落下就是捏合，调选中的按钮的大小；再多的手指不管 */
    fun down(id: Int, x: Float, y: Float) {
        fingers[id] = x to y
        if (fingers.size == 2) {
            startPinch()
            return
        }
        if (fingers.size > 2 || pointer != null) return
        pointer = id
        startX = x
        startY = y
        moved = false
        val c = geo.hit(layout.controls, x, y) { 6 * geo.density }
        dragging = c?.id
        if (c != null) {
            selected = c.id
            settings = false
            grabX = x - geo.cx(c)
            grabY = y - geo.cy(c)
            before = layout
        }
        redraw()
    }

    fun move(id: Int, x: Float, y: Float) {
        if (id in fingers) fingers[id] = x to y
        pinch?.let {
            resize(it)
            return
        }
        if (id != pointer) return
        val c = dragging?.let(layout::control) ?: return
        if (!moved && hypot(x - startX, y - startY) < slop) return
        moved = true
        val others = layout.controls.filter { it.id != c.id }
        val snap = SNAP_DP * geo.density
        val sx = nearest(x - grabX, others.map(geo::cx) + (geo.left + geo.width / 2), snap)
        val sy = nearest(y - grabY, others.map(geo::cy) + (geo.top + geo.height / 2), snap)
        val (fx, fy) = geo.place(c.size, sx ?: (x - grabX), sy ?: (y - grabY))
        val placed = c.at(fx, fy)
        // 被拉回区域里了就不在辅助线上了
        val gx = sx?.takeIf { abs(geo.cx(placed) - it) < 0.5f }
        val gy = sy?.takeIf { abs(geo.cy(placed) - it) < 0.5f }
        if ((gx != null && guideX == null) || (gy != null && guideY == null)) tick()
        guideX = gx
        guideY = gy
        layout = layout.with(placed)
        redraw()
    }

    fun up(id: Int) {
        fingers.remove(id)
        pinch?.let {
            // 松开一个手指就结束捏合；剩下的手指不再拖动，等全部抬起
            endPinch(it)
            if (fingers.isEmpty()) reset()
            return
        }
        if (id != pointer) return
        if (moved) {
            before?.let(::push)
        } else if (dragging == null) {
            // 点了空白处：取消选中、收起设置
            selected = null
            settings = false
        }
        reset()
    }

    /** 触摸被打断：拖到哪里、捏到多大算多大 */
    fun cancel() {
        pinch?.let(::endPinch)
        if (moved) before?.let(::push)
        fingers.clear()
        reset()
    }

    /** 第二个手指落下：调拖着的按钮，没在拖就调选中的。正在拖的先算一步撤销 */
    private fun startPinch() {
        val id = dragging ?: selected ?: return
        val c = layout.control(id) ?: return
        if (moved) before?.let(::push)
        val (a, b) = fingers.values.toList()
        val d = hypot(a.first - b.first, a.second - b.second)
        if (d < 1f) return
        pinch = Pinch(id, c.size, d, layout)
        selected = id
        // 第一个手指不再拖动
        pointer = null
        dragging = null
        moved = false
        before = null
        guideX = null
        guideY = null
        redraw()
    }

    private fun resize(p: Pinch) {
        if (fingers.size < 2) return
        val c = layout.control(p.id) ?: return
        val (a, b) = fingers.values.toList()
        val d = hypot(a.first - b.first, a.second - b.second)
        val next = geo.inside(c.sized(p.size * d / p.dist))
        if (next == c) return
        layout = layout.with(next)
        redraw()
    }

    private fun endPinch(p: Pinch) {
        if (layout != p.before) push(p.before)
        pinch = null
    }

    /** 正在捏合调大小的按钮（画的时候突出） */
    val resizing: Int? get() = pinch?.id

    private fun reset() {
        pointer = null
        dragging = null
        moved = false
        before = null
        guideX = null
        guideY = null
        redraw()
    }

    private fun nearest(v: Float, lines: List<Float>, within: Float): Float? =
        lines.minByOrNull { abs(it - v) }?.takeIf { abs(it - v) <= within }

    companion object {
        /** 离辅助线多近（dp）就吸过去 */
        const val SNAP_DP = 8f
        const val MAX_UNDO = 50
    }
}

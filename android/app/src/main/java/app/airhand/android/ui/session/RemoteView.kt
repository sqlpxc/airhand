package app.airhand.android.ui.session

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Matrix
import android.graphics.Paint
import android.graphics.Path
import android.graphics.Rect
import android.graphics.RectF
import android.graphics.SurfaceTexture
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.HapticFeedbackConstants
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.Surface
import android.view.TextureView
import android.view.View
import android.view.ViewConfiguration
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.widget.FrameLayout
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import app.airhand.android.controller.CursorState
import app.airhand.android.controller.Input
import app.airhand.android.data.GameLayout
import app.airhand.android.data.GameStick
import app.airhand.android.data.ScaleMode
import kotlin.math.abs
import kotlin.math.max
import kotlin.math.min

/**
 * 远程画面：TextureView 显示解码出来的画面，上面一层画指针和点按反馈。
 *
 * * 画面按比例缩放到视图里（[content] 是画面在视图里的位置），可以双指放大 / 移动；
 * * 触摸交给 [Gestures]；外接鼠标、键盘直接对应远端的鼠标、键盘；
 * * 被控端的光标隐藏了（游戏里转视角等）或者打开了锁定鼠标：发相对移动（[relative]）。
 *   外接鼠标这时被抓住（Android 8+ 的 pointer capture：系统指针隐藏、不会碰到屏幕边停下），
 *   发鼠标的原始移动量；触摸按画面大小换算成被控端的像素；
 * * 软键盘的输入经 [RemoteInputConnection] 交给 [KeyboardController]；
 * * 开着游戏按键（[setGame]）时，落在按钮上的手指归 [GameControls]，其余的手指照常交给 [Gestures]
 *   （[split]：左手按着摇杆、右手在空白处转视角，对 Gestures 来说只有一个手指）；编辑方案时（[editor]）
 *   触摸都交给 [LayoutEditor]，不发给对方。
 */
@SuppressLint("ViewConstructor")
class RemoteView(context: Context, private val host: Host) : FrameLayout(context), TextureView.SurfaceTextureListener {

    interface Host {
        fun onSurface(surface: Surface?)

        /** 画面放大了 / 缩回去了：整个画面现在显示多大（本机像素），用来向对方要更清楚的画面 */
        fun onZoom(zoomed: Boolean, shownW: Float, shownH: Float)
        fun onInput(events: List<Any>, now: Boolean)
        fun onToggleKeyboard()
        val keyboard: KeyboardController
    }

    private val texture = TextureView(context)
    private val overlay = Overlay(context)
    private val gameLayer = GameLayer(context, this)
    private var surface: Surface? = null

    private var videoW = 0
    private var videoH = 0
    private val content = RectF()
    private val density = resources.displayMetrics.density

    /** 只看画面（电视）：不处理触摸、不发送输入 */
    var interactive = true

    /** 弹出软键盘时为 true（这时视图才是一个"编辑框"） */
    var keyboardWanted = false

    private val main = Handler(Looper.getMainLooper())
    private val timers = object : Gestures.Timers {
        override fun after(ms: Long, r: () -> Unit): Any {
            val runnable = Runnable { r() }
            main.postDelayed(runnable, ms)
            return runnable
        }

        override fun cancel(token: Any?) {
            (token as? Runnable)?.let { main.removeCallbacks(it) }
        }
    }

    val gestures = Gestures(object : Gestures.Out {
        override fun toRemote(x: Float, y: Float): Pair<Float, Float>? {
            if (content.width() <= 0f || content.height() <= 0f) return null
            return ((x - content.left) / content.width()).coerceIn(0f, 1f) to ((y - content.top) / content.height()).coerceIn(0f, 1f)
        }

        override fun contentSize() = content.width() to content.height()
        override fun move(nx: Float, ny: Float) {
            overlay.invalidate()
            host.onInput(listOf(Input.move(nx, ny)), false)
        }

        override fun delta(nx: Float, ny: Float) {
            // 画面宽 / 高的几分之几 → 被控端的像素（不知道对方屏幕多大时按画面的大小）
            val (w, h) = remoteSize ?: (videoW to videoH)
            sendDelta(nx * w, ny * h)
        }

        override fun button(button: String, down: Boolean) = host.onInput(listOf(Input.button(button, down)), true)
        override fun wheel(dx: Int, dy: Int) = host.onInput(listOf(Input.wheel(dx, dy)), false)
        override fun zoom(factor: Float, fx: Float, fy: Float) = zoomBy(factor, fx, fy)
        override fun pan(dx: Float, dy: Float) = panBy(dx, dy)
        override fun follow() = followCursor()
        override fun haptic() {
            performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
        }

        override fun ripple(x: Float, y: Float) = overlay.ripple(x, y)
        override fun toggleKeyboard() = host.onToggleKeyboard()

        // 按下 / 抬起马上发，移动和别的移动一起合并着发（8 ms 一批）
        override fun touch(id: Int, phase: String, nx: Float, ny: Float) =
            host.onInput(listOf(Input.touch(id, phase, nx, ny)), phase != Gestures.MOVE)
    }, timers, ViewConfiguration.get(context).scaledTouchSlop.toFloat())

    private val touchSlop = ViewConfiguration.get(context).scaledTouchSlop.toFloat()

    /** 游戏按钮可以放的地方：视图去掉刘海，留一点边（[updateGameArea]） */
    private val gameGeo = GameGeometry(density)

    /** 游戏按键发出去的输入：按键、点击马上发；转视角和别的相对移动一样换算、合并 */
    val game: GameControls = GameControls(object : GameControls.Out {
        override fun key(code: String, down: Boolean) = host.onInput(listOf(Input.key(code, down)), true)
        override fun button(button: String, down: Boolean) = host.onInput(listOf(Input.button(button, down)), true)
        override fun wheel(dy: Int) = host.onInput(listOf(Input.wheel(0, dy)), true)
        override fun look(dx: Float, dy: Float) = lookBy(dx, dy)
        override fun haptic() {
            performHapticFeedback(HapticFeedbackConstants.VIRTUAL_KEY)
        }

        override fun changed() {
            gameLayer.invalidate()
            // 按着鼠标键的按钮（开火、右键转视角）和按着外接鼠标的键一样算（updateRelative）
            if (gameMouse != game.holdsMouse) {
                gameMouse = game.holdsMouse
                updateRelative()
            }
        }
    }, timers, gameGeo, touchSlop)

    private var gameMouse = false

    /** 转视角的灵敏度（方案里的） */
    private var lookGain = 1f

    /** 正在编辑的方案：触摸都交给它，不发给对方 */
    var editor: LayoutEditor? = null
        set(v) {
            if (field === v) return
            field = v
            if (v != null) {
                game.releaseAll()
                gestures.cancel()
                freeIds.clear()
            }
            updateCapture()
            gameLayer.invalidate()
        }

    init {
        setBackgroundColor(Color.BLACK)
        texture.surfaceTextureListener = this
        addView(texture, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT))
        addView(overlay, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT))
        addView(gameLayer, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT))
        isFocusable = true
        isFocusableInTouchMode = true
    }

    // ───────────────────────────── 画面 ─────────────────────────────

    override fun onSurfaceTextureAvailable(st: SurfaceTexture, width: Int, height: Int) {
        surface = Surface(st)
        host.onSurface(surface)
    }

    override fun onSurfaceTextureSizeChanged(st: SurfaceTexture, width: Int, height: Int) {}

    override fun onSurfaceTextureDestroyed(st: SurfaceTexture): Boolean {
        host.onSurface(null)
        surface?.release()
        surface = null
        return true
    }

    override fun onSurfaceTextureUpdated(st: SurfaceTexture) {}

    fun setVideoSize(w: Int, h: Int) {
        if (w == videoW && h == videoH) return
        // 放大着、画面只是换了分辨率（放大之后向对方要了更清楚的画面，宽高比没变）：保持放大倍数和位置
        val keep = zoomed && scaleMode == ScaleMode.FIT && w > 0 && h > 0 && videoW > 0 && videoH > 0 &&
            content.width() > 0f && abs(w.toFloat() / h - videoW.toFloat() / videoH) < 0.02f * videoW / videoH
        val cx = if (keep) (width / 2f - content.left) / content.width() else 0f
        val cy = if (keep) (height / 2f - content.top) / content.height() else 0f
        videoW = w
        videoH = h
        if (keep) zoomAround(cx, cy) else fit()
    }

    /**
     * 画面怎样放进视图（电视用）：完整显示 / 铺满（裁掉超出的部分）/ 拉伸。
     * 只有完整显示时才能双指放大（手机一直是完整显示）
     */
    var scaleMode = ScaleMode.FIT
        set(v) {
            if (field == v) return
            field = v
            fit()
        }

    /** 画面恢复为没有放大时的样子（按 [scaleMode]） */
    fun fit() {
        if (videoW == 0 || videoH == 0 || width == 0 || height == 0) return
        if (scaleMode == ScaleMode.STRETCH) {
            content.set(0f, 0f, width.toFloat(), height.toFloat())
        } else {
            val sx = width.toFloat() / videoW
            val sy = height.toFloat() / videoH
            val s = if (scaleMode == ScaleMode.FILL) max(sx, sy) else min(sx, sy)
            val w = videoW * s
            val h = videoH * s
            content.set((width - w) / 2, (height - h) / 2, (width + w) / 2, (height + h) / 2)
        }
        rel = 1f
        apply()
    }

    /** 用户放大的倍数（相对于完整显示） */
    private var rel = 1f

    val zoomed: Boolean get() = rel > 1.01f

    private fun fitScale(): Float =
        if (videoW == 0 || videoH == 0 || width == 0) 0f else min(width.toFloat() / videoW, height.toFloat() / videoH)

    override fun onLayout(changed: Boolean, left: Int, top: Int, right: Int, bottom: Int) {
        super.onLayout(changed, left, top, right, bottom)
        updateGameArea()
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        super.onSizeChanged(w, h, oldw, oldh)
        if (!zoomed || scaleMode != ScaleMode.FIT || oldw == 0 || oldh == 0 || content.width() <= 0f) {
            fit()
            return
        }
        // 放大着（例如弹出键盘、旋转）：保持放大倍数，视图中心对着的还是原来那一点
        zoomAround((oldw / 2f - content.left) / content.width(), (oldh / 2f - content.top) / content.height())
    }

    /** 保持放大倍数 [rel]，画面上的 (`cx`, `cy`)（相对位置）对着视图中心 */
    private fun zoomAround(cx: Float, cy: Float) {
        val s = fitScale() * rel
        val cw = videoW * s
        val ch = videoH * s
        content.set(width / 2f - cx * cw, height / 2f - cy * ch, width / 2f - cx * cw + cw, height / 2f - cy * ch + ch)
        clamp()
        apply()
    }

    private fun zoomBy(factor: Float, fx: Float, fy: Float) {
        val base = fitScale()
        if (base <= 0f || scaleMode != ScaleMode.FIT) return
        val cur = content.width() / videoW
        val next = (cur * factor).coerceIn(base, base * MAX_ZOOM)
        rel = next / base
        val f = next / cur
        content.set(
            fx - (fx - content.left) * f,
            fy - (fy - content.top) * f,
            fx - (fx - content.left) * f + content.width() * f,
            fy - (fy - content.top) * f + content.height() * f,
        )
        clamp()
        apply()
    }

    private fun panBy(dx: Float, dy: Float) {
        content.offset(dx, dy)
        clamp()
        apply()
    }

    /** 画面比视图小的方向居中，比视图大的方向不留黑边 */
    private fun clamp() {
        val w = width.toFloat()
        val h = height.toFloat()
        if (content.width() <= w) {
            content.offsetTo((w - content.width()) / 2, content.top)
        } else {
            if (content.left > 0) content.offset(-content.left, 0f)
            if (content.right < w) content.offset(w - content.right, 0f)
        }
        if (content.height() <= h) {
            content.offsetTo(content.left, (h - content.height()) / 2)
        } else {
            if (content.top > 0) content.offset(0f, -content.top)
            if (content.bottom < h) content.offset(0f, h - content.bottom)
        }
    }

    /** 触控板模式放大时：指针快到边上了就挪动画面 */
    private fun followCursor() {
        if (!zoomed) return
        val cx = content.left + gestures.cursorX * content.width()
        val cy = content.top + gestures.cursorY * content.height()
        val mx = width * 0.15f
        val my = height * 0.15f
        var dx = 0f
        var dy = 0f
        if (cx < mx) dx = mx - cx else if (cx > width - mx) dx = width - mx - cx
        if (cy < my) dy = my - cy else if (cy > height - my) dy = height - my - cy
        if (dx != 0f || dy != 0f) panBy(dx, dy)
    }

    private fun apply() {
        if (width == 0 || height == 0) return
        val m = Matrix()
        m.setScale(content.width() / width, content.height() / height)
        m.postTranslate(content.left, content.top)
        texture.setTransform(m)
        overlay.invalidate()
        host.onZoom(zoomed, content.width(), content.height())
    }

    // ───────────────────────────── 指针 ─────────────────────────────

    /** 本机画不画远端指针：触控板 / 触屏模式画，直通（控制手机）和电视不画 */
    var showCursor = true
        set(v) {
            field = v
            overlay.invalidate()
        }

    /** 对方把光标画进了画面里时（[CursorState.inFrame]）也画本机的：跟手，只是有两个（设置里的 localCursor） */
    var localCursor = true
        set(v) {
            field = v
            overlay.invalidate()
        }

    private var cursor = CursorState()

    fun setCursor(c: CursorState) {
        cursor = c
        updateRelative()
        overlay.invalidate()
    }

    // ───────────────────────────── 相对移动 ─────────────────────────────

    /** 锁定鼠标：不管被控端的光标是否隐藏，一直发相对移动 */
    var mouseLock = false
        set(v) {
            field = v
            updateRelative()
        }

    /** 被控端当前显示器的大小（像素），触摸的相对移动按它换算 */
    private var remoteSize: Pair<Int, Int>? = null

    fun setRemoteSize(w: Int, h: Int) {
        remoteSize = if (w > 0 && h > 0) w to h else null
    }

    /** 还不够一个像素的移动量 */
    private var fracX = 0f
    private var fracY = 0f

    /**
     * 按着键（外接鼠标的键、触摸拖动）期间被控端隐藏过光标：松开之前一直当作隐藏。这是防御性的：万一转视角时
     * 光标隐藏 / 出现来回跳（被控端的检测抖动、个别游戏自己在闪），跟着它改回发位置，视角会乱跳，外接鼠标也来回
     * 抓住、放开。实际游戏里还没见过这样跳（以前魔兽世界里的反复锁定是控制台把按着的键当成了松开，见
     * client/src/lib/input.ts 的 buttonsAfter）。代价：游戏在按着键时真的把光标显示出来，要等松开才改回发位置
     */
    private var hiddenWhilePressed = false

    /** 发相对移动：被控端的光标隐藏了（按着键时见 [hiddenWhilePressed]），或者打开了锁定鼠标。按键变了也要调用 */
    private fun updateRelative() {
        val pressed = mouseButtons != 0 || gestures.pressing || game.holdsMouse
        val held = pressed && (hiddenWhilePressed || !cursor.visible)
        if (held != hiddenWhilePressed) {
            hiddenWhilePressed = held
            overlay.invalidate()
        }
        val r = interactive && (mouseLock || !cursor.visible || hiddenWhilePressed)
        if (gestures.relative != r) {
            gestures.relative = r
            fracX = 0f
            fracY = 0f
        }
        updateCapture()
    }

    private fun sendDelta(dx: Float, dy: Float) {
        fracX += dx
        fracY += dy
        val ix = fracX.toInt()
        val iy = fracY.toInt()
        if (ix == 0 && iy == 0) return
        fracX -= ix
        fracY -= iy
        host.onInput(listOf(Input.delta(ix, iy)), false)
    }

    /** 游戏按键转视角：手指移动了多少像素，和触屏拖动一样按画面大小换算，再乘上灵敏度 */
    private fun lookBy(dx: Float, dy: Float) {
        if (content.width() <= 0f || content.height() <= 0f) return
        val (w, h) = remoteSize ?: (videoW to videoH)
        sendDelta(dx / content.width() * w * lookGain, dy / content.height() * h * lookGain)
    }

    /** 用过外接鼠标（才去抓鼠标：只用触摸屏时没必要） */
    private var mouseSeen = false

    /** 相对移动时抓住外接鼠标：系统指针隐藏，鼠标事件都交给 [onCapturedPointerEvent]（原始移动量） */
    private fun updateCapture() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val want = gestures.relative && mouseSeen && hasWindowFocus() && editor == null
        if (want && !hasPointerCapture()) {
            requestPointerCapture()
        } else if (!want && hasPointerCapture()) {
            releasePointerCapture()
        }
    }

    override fun onCapturedPointerEvent(e: MotionEvent): Boolean {
        if (!interactive) return false
        if (e.actionMasked == MotionEvent.ACTION_SCROLL) {
            scroll(e)
            return true
        }
        // 抓住时的坐标就是移动量（一个事件里可能带着几次采样）
        var dx = e.x
        var dy = e.y
        for (i in 0 until e.historySize) {
            dx += e.getHistoricalX(i)
            dy += e.getHistoricalY(i)
        }
        if (dx != 0f || dy != 0f) sendDelta(dx, dy)
        mouseButtons(e)
        return true
    }

    private inner class Overlay(context: Context) : View(context) {
        private val paint = Paint(Paint.ANTI_ALIAS_FLAG or Paint.FILTER_BITMAP_FLAG)
        private val arrow = Path().apply {
            // 默认箭头（对方还没发光标形状时）
            moveTo(0f, 0f); lineTo(0f, 17f); lineTo(4.2f, 13f); lineTo(7f, 19.5f); lineTo(9.6f, 18.4f)
            lineTo(6.9f, 12.2f); lineTo(12.4f, 12.2f); close()
        }
        private val ripples = ArrayList<Triple<Float, Float, Long>>()

        fun ripple(x: Float, y: Float) {
            ripples += Triple(x, y, SystemClock.uptimeMillis())
            invalidate()
        }

        override fun onDraw(canvas: Canvas) {
            val now = SystemClock.uptimeMillis()
            // 点按反馈：一个淡出的圆
            val it = ripples.iterator()
            while (it.hasNext()) {
                val (x, y, t) = it.next()
                val k = (now - t) / 350f
                if (k >= 1f) {
                    it.remove()
                    continue
                }
                paint.style = Paint.Style.FILL
                paint.color = Color.WHITE
                paint.alpha = ((1f - k) * 90).toInt()
                canvas.drawCircle(x, y, (18 + 22 * k) * density, paint)
            }
            if (ripples.isNotEmpty()) postInvalidateOnAnimation()

            // 光标已经画在画面里了（对方拖动、缩放窗口等）：默认照样画（跟手），设置里关掉了就只看画面里的
            if (!showCursor || !cursor.visible || hiddenWhilePressed || (cursor.inFrame && !localCursor) || content.width() <= 0f) return
            val x = content.left + gestures.cursorX * content.width()
            val y = content.top + gestures.cursorY * content.height()
            val bmp: Bitmap? = cursor.bitmap
            if (bmp != null) {
                // 按画面的缩放比例画，但不小于 22dp、不大于 44dp
                val s = (content.width() / max(1, videoW)).coerceIn(22 * density / bmp.height, 44 * density / bmp.height)
                canvas.save()
                canvas.translate(x - cursor.hotX * s, y - cursor.hotY * s)
                canvas.scale(s, s)
                paint.alpha = 255
                canvas.drawBitmap(bmp, 0f, 0f, paint)
                canvas.restore()
            } else {
                canvas.save()
                canvas.translate(x, y)
                canvas.scale(density * 1.15f, density * 1.15f)
                paint.alpha = 255
                paint.style = Paint.Style.FILL
                paint.color = Color.WHITE
                canvas.drawPath(arrow, paint)
                paint.style = Paint.Style.STROKE
                paint.strokeWidth = 1.2f
                paint.color = Color.BLACK
                canvas.drawPath(arrow, paint)
                canvas.restore()
            }
        }
    }

    // ───────────────────────────── 触摸 / 鼠标 ─────────────────────────────

    private var mouseButtons = 0

    @SuppressLint("ClickableViewAccessibility")
    override fun onTouchEvent(e: MotionEvent): Boolean {
        if (!interactive) return false
        editor?.let {
            edit(it, e)
            return true
        }
        if (e.isFromSource(InputDevice.SOURCE_MOUSE)) return mouse(e)
        val wasPressing = gestures.pressing
        if (game.layout != null) split(e) else touch(e)
        if (gestures.pressing != wasPressing) updateRelative()
        return true
    }

    private fun touch(e: MotionEvent) {
        val t = e.eventTime
        fun pt(i: Int) = Gestures.P(e.getPointerId(i), e.getX(i), e.getY(i))
        fun pts() = (0 until e.pointerCount).map { pt(it) }
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                gestures.down(pt(0), t)
                overlay.invalidate()
            }
            MotionEvent.ACTION_POINTER_DOWN -> gestures.pointerDown(pt(e.actionIndex), pts(), t)
            MotionEvent.ACTION_MOVE -> gestures.move(pts(), t)
            MotionEvent.ACTION_POINTER_UP -> gestures.pointerUp(pt(e.actionIndex), e.pointerCount - 1, t)
            MotionEvent.ACTION_UP -> gestures.up(pt(e.actionIndex), t)
            MotionEvent.ACTION_CANCEL -> gestures.cancel()
        }
    }

    /** 交给 [gestures] 的手指（没有落在游戏按钮上的），按落下的先后 */
    private val freeIds = ArrayList<Int>()

    /**
     * 开着游戏按键时分手指：落在按钮上的从按下到抬起都归 [game]，其余的交给 [gestures]。
     * Gestures 只看得到它自己的手指：第一个是 `down`，之后的是 `pointerDown`，最后一个抬起是 `up`
     */
    private fun split(e: MotionEvent) {
        val t = e.eventTime
        fun pt(i: Int) = Gestures.P(e.getPointerId(i), e.getX(i), e.getY(i))
        fun free() = (0 until e.pointerCount).map(::pt).filter { it.id in freeIds }
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
                if (e.actionMasked == MotionEvent.ACTION_DOWN) {
                    // 新的一次触摸：上一次漏掉的抬起（不应该发生）不再算
                    game.lift()
                    freeIds.clear()
                }
                val p = pt(e.actionIndex)
                if (game.down(p.id, p.x, p.y)) return
                freeIds += p.id
                val ps = free()
                if (ps.size == 1) {
                    gestures.down(p, t)
                    overlay.invalidate()
                } else {
                    gestures.pointerDown(p, ps, t)
                }
            }
            MotionEvent.ACTION_MOVE -> {
                for (i in 0 until e.pointerCount) {
                    val id = e.getPointerId(i)
                    if (game.owns(id)) game.move(id, e.getX(i), e.getY(i))
                }
                val ps = free()
                if (ps.isNotEmpty()) gestures.move(ps, t)
            }
            MotionEvent.ACTION_POINTER_UP, MotionEvent.ACTION_UP -> {
                val p = pt(e.actionIndex)
                if (game.up(p.id)) return
                if (freeIds.remove(p.id)) {
                    if (freeIds.isEmpty()) gestures.up(p, t) else gestures.pointerUp(p, freeIds.size, t)
                }
            }
            MotionEvent.ACTION_CANCEL -> {
                game.lift()
                if (freeIds.isNotEmpty()) gestures.cancel()
                freeIds.clear()
            }
        }
    }

    /** 编辑方案：手指（和外接鼠标）都用来摆放按钮 */
    private fun edit(ed: LayoutEditor, e: MotionEvent) {
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN ->
                ed.down(e.getPointerId(e.actionIndex), e.getX(e.actionIndex), e.getY(e.actionIndex))
            MotionEvent.ACTION_MOVE -> for (i in 0 until e.pointerCount) ed.move(e.getPointerId(i), e.getX(i), e.getY(i))
            MotionEvent.ACTION_POINTER_UP, MotionEvent.ACTION_UP -> ed.up(e.getPointerId(e.actionIndex))
            MotionEvent.ACTION_CANCEL -> ed.cancel()
        }
    }

    // ───────────────────────────── 游戏按键 ─────────────────────────────

    /**
     * 显示游戏按键用的方案；null 收起。换了、收起都先把按着的松开；开 / 关之间正在进行的手势作废
     * （手指不会一半算按钮、一半算手势）
     */
    fun setGame(layout: GameLayout?) {
        if ((layout == null) != (game.layout == null)) {
            gestures.cancel()
            freeIds.clear()
        }
        game.layout = layout
        lookGain = layout?.look ?: 1f
        gestures.lookGain = layout?.look
        updateGestureExclusion()
    }

    /**
     * 摇杆、贴着屏幕边的按钮不让系统的返回手势抢走（Android 10+ 的手势导航：从左右边缘滑入是返回，
     * 拇指推摇杆时很容易碰到）。系统每条边最多让出 200dp，从下往上算，摇杆一般在下面
     */
    private fun updateGestureExclusion() {
        val l = game.layout
        val rects = if (l == null || !gameGeo.ready) emptyList() else l.controls.map { c ->
            val r = gameGeo.radius(c) * (if (c is GameStick) 1.3f else 1.15f)
            val x = gameGeo.cx(c)
            val y = gameGeo.cy(c)
            Rect((x - r).toInt(), (y - r).toInt(), (x + r).toInt(), (y + r).toInt())
        }
        ViewCompat.setSystemGestureExclusionRects(this, rects)
    }

    /** 开始编辑一个方案（设到 [editor] 才生效） */
    fun newEditor(layout: GameLayout): LayoutEditor =
        LayoutEditor(layout, gameGeo, touchSlop, redraw = { gameLayer.invalidate() }, tick = {
            performHapticFeedback(HapticFeedbackConstants.CLOCK_TICK)
        })

    /** 按钮可以放的地方：去掉刘海（画面铺到了刘海里），再留 6dp 的边 */
    private fun updateGameArea() {
        val cut = ViewCompat.getRootWindowInsets(this)?.getInsets(WindowInsetsCompat.Type.displayCutout())
        val m = 6 * density
        gameGeo.set(
            (cut?.left ?: 0) + m,
            (cut?.top ?: 0) + m,
            width - (cut?.right ?: 0) - m,
            height - (cut?.bottom ?: 0) - m,
        )
        updateGestureExclusion()
        gameLayer.invalidate()
    }

    override fun onGenericMotionEvent(e: MotionEvent): Boolean {
        if (!interactive || !e.isFromSource(InputDevice.SOURCE_CLASS_POINTER)) return super.onGenericMotionEvent(e)
        // 编辑方案时不操作对方
        if (editor != null) return true
        when (e.actionMasked) {
            MotionEvent.ACTION_HOVER_MOVE -> {
                moveMouse(e)
                return true
            }
            MotionEvent.ACTION_SCROLL -> {
                scroll(e)
                return true
            }
        }
        return super.onGenericMotionEvent(e)
    }

    private fun scroll(e: MotionEvent) {
        val dy = (e.getAxisValue(MotionEvent.AXIS_VSCROLL) * 120).toInt()
        val dx = (e.getAxisValue(MotionEvent.AXIS_HSCROLL) * 120).toInt()
        if (dx != 0 || dy != 0) host.onInput(listOf(Input.wheel(dx, dy)), true)
    }

    private fun moveMouse(e: MotionEvent) {
        if (!mouseSeen && e.isFromSource(InputDevice.SOURCE_MOUSE)) {
            mouseSeen = true
            updateCapture()
        }
        if (gestures.relative) {
            // 还没抓住鼠标（Android 7、窗口刚拿到焦点）：用鼠标的原始移动量
            val dx = e.getAxisValue(MotionEvent.AXIS_RELATIVE_X)
            val dy = e.getAxisValue(MotionEvent.AXIS_RELATIVE_Y)
            if (dx != 0f || dy != 0f) sendDelta(dx, dy)
            return
        }
        if (content.width() <= 0f || content.height() <= 0f) return
        val nx = ((e.x - content.left) / content.width()).coerceIn(0f, 1f)
        val ny = ((e.y - content.top) / content.height()).coerceIn(0f, 1f)
        gestures.setCursor(nx, ny)
        overlay.invalidate()
        host.onInput(listOf(Input.move(nx, ny)), false)
    }

    /** 外接鼠标：位置一一对应（相对移动时发移动量），按键原样发过去 */
    private fun mouse(e: MotionEvent): Boolean {
        moveMouse(e)
        mouseButtons(e)
        return true
    }

    /** 鼠标按键变了：发按下 / 松开 */
    private fun mouseButtons(e: MotionEvent) {
        val now = if (e.actionMasked == MotionEvent.ACTION_UP || e.actionMasked == MotionEvent.ACTION_CANCEL) 0 else e.buttonState
        val changed = now xor mouseButtons
        val events = ArrayList<Any>()
        for ((bit, name) in MOUSE_BUTTONS) {
            if (changed and bit != 0) events += Input.button(name, now and bit != 0)
        }
        mouseButtons = now
        if (events.isNotEmpty()) host.onInput(events, true)
        // 松开最后一个键（照上面的方式发出去之后）：按着键期间光标出现了的，现在改回发位置
        if (changed != 0) updateRelative()
    }

    // ───────────────────────────── 键盘 ─────────────────────────────

    override fun onCheckIsTextEditor(): Boolean = keyboardWanted

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection? {
        if (!keyboardWanted) return null
        RemoteInputConnection.configure(outAttrs)
        return RemoteInputConnection(this, host.keyboard)
    }

    private var showPending = false

    /** 弹出软键盘。菜单（另一个窗口）刚关上时窗口还没拿回焦点，等拿到焦点再弹 */
    fun showKeyboard() {
        keyboardWanted = true
        requestFocus()
        if (hasWindowFocus()) doShowKeyboard() else showPending = true
    }

    private fun doShowKeyboard() {
        val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        imm.restartInput(this)
        imm.showSoftInput(this, 0)
        (context as? android.app.Activity)?.window?.let {
            WindowCompat.getInsetsController(it, this).show(WindowInsetsCompat.Type.ime())
        }
    }

    override fun onWindowFocusChanged(hasWindowFocus: Boolean) {
        super.onWindowFocusChanged(hasWindowFocus)
        if (hasWindowFocus && showPending && keyboardWanted) {
            showPending = false
            post { doShowKeyboard() }
        }
        // 拿到焦点才能抓住鼠标（失去焦点时系统自己会放开）
        updateCapture()
        // 菜单、通知栏、切到后台：游戏按键按着的都松开（手指的抬起不一定会送到这里），切换着的也松开，
        // 不然角色会一直走下去
        if (!hasWindowFocus) game.releaseAll()
    }

    fun hideKeyboard() {
        showPending = false
        val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        imm.hideSoftInputFromWindow(windowToken, 0)
        keyboardWanted = false
    }

    /** 外接键盘：按键原样发过去（返回、音量键留给本机） */
    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean =
        if (interactive && passKey(event)) host.keyboard.key(event) else super.onKeyDown(keyCode, event)

    override fun onKeyUp(keyCode: Int, event: KeyEvent): Boolean =
        if (interactive && passKey(event)) host.keyboard.key(event) else super.onKeyUp(keyCode, event)

    private fun passKey(e: KeyEvent): Boolean {
        if (e.keyCode in LOCAL_KEYS) return false
        return e.device?.isVirtual == false || keyboardWanted
    }

    companion object {
        const val MAX_ZOOM = 5f
        private val MOUSE_BUTTONS = listOf(
            MotionEvent.BUTTON_PRIMARY to Input.LEFT,
            MotionEvent.BUTTON_SECONDARY to Input.RIGHT,
            MotionEvent.BUTTON_TERTIARY to Input.MIDDLE,
            MotionEvent.BUTTON_BACK to Input.BACK,
            MotionEvent.BUTTON_FORWARD to Input.FORWARD,
        )
        private val LOCAL_KEYS = setOf(
            KeyEvent.KEYCODE_BACK, KeyEvent.KEYCODE_VOLUME_UP, KeyEvent.KEYCODE_VOLUME_DOWN, KeyEvent.KEYCODE_VOLUME_MUTE,
            KeyEvent.KEYCODE_POWER, KeyEvent.KEYCODE_HOME, KeyEvent.KEYCODE_APP_SWITCH,
        )
    }
}

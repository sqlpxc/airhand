package app.airhand.android.ui.session

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.DashPathEffect
import android.graphics.Paint
import android.graphics.Typeface
import android.view.View
import app.airhand.android.data.GameControl
import app.airhand.android.data.GameKey
import app.airhand.android.data.GameLayout
import app.airhand.android.data.GameStick
import app.airhand.android.data.KeyMode
import kotlin.math.max

/**
 * 画游戏按键（盖在远程画面上，不接触摸：触摸由 [RemoteView] 分给 [GameControls] / [LayoutEditor]）。
 *
 * * 平时：半透明的圆按钮，按下时变成主题色、稍微缩一点；切换着的按钮外面一圈主题色；
 *   能转视角的按钮下面有个小准星，点一下切换的按钮上面有个小圆点；
 *   摇杆是一个大圆加四个方向的键名，按着时圆心跟着手指落下的地方，按着的方向亮起来，推出边缘时外圈亮起来；
 * * 编辑时：画面压暗、铺一层点阵，按钮都有虚线框，选中的是主题色的框，拖动时画出对齐的辅助线
 */
@SuppressLint("ViewConstructor")
class GameLayer(context: Context, private val view: RemoteView) : View(context) {
    private val d = resources.displayMetrics.density
    private val fill = Paint(Paint.ANTI_ALIAS_FLAG).apply { style = Paint.Style.FILL }
    private val line = Paint(Paint.ANTI_ALIAS_FLAG).apply { style = Paint.Style.STROKE }
    private val text = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        textAlign = Paint.Align.CENTER
        typeface = Typeface.create(Typeface.DEFAULT, Typeface.BOLD)
        color = Color.WHITE
    }
    private val dash = DashPathEffect(floatArrayOf(5 * d, 4 * d), 0f)
    private val str: (Int) -> String = { context.getString(it) }

    /** 编辑时的点阵（按大小算好） */
    private var dots = FloatArray(0)
    private var dotsFor = 0L

    override fun onDraw(canvas: Canvas) {
        val ed = view.editor
        if (ed != null) {
            drawEditor(canvas, ed)
            return
        }
        val l = view.game.layout ?: return
        val geo = view.game.geo
        if (!geo.ready) return
        for (c in l.controls) {
            when (c) {
                is GameKey -> drawKey(canvas, c, l.opacity, pressed = view.game.isDown(c.id))
                is GameStick -> drawStick(canvas, c, l.opacity, view.game.stickState(c.id))
            }
        }
    }

    private fun drawEditor(canvas: Canvas, ed: LayoutEditor) {
        val geo = view.game.geo
        canvas.drawColor(Color.argb(110, 0, 0, 0))
        if (!geo.ready) return
        // 点阵：每 24dp 一个点，摆放时有个参照
        val key = (width.toLong() shl 32) or height.toLong()
        if (dotsFor != key) {
            val step = 24 * d
            val pts = ArrayList<Float>()
            var y = step / 2
            while (y < height) {
                var x = step / 2
                while (x < width) {
                    pts += x
                    pts += y
                    x += step
                }
                y += step
            }
            dots = pts.toFloatArray()
            dotsFor = key
        }
        line.pathEffect = null
        line.strokeCap = Paint.Cap.ROUND
        line.strokeWidth = 2 * d
        line.color = Color.argb(60, 255, 255, 255)
        canvas.drawPoints(dots, line)
        // 可以放按钮的区域（去掉刘海）
        line.strokeWidth = 1 * d
        line.pathEffect = dash
        line.color = Color.argb(70, 255, 255, 255)
        canvas.drawRect(geo.left, geo.top, geo.left + geo.width, geo.top + geo.height, line)

        val l = ed.layout
        for (c in l.controls) {
            when (c) {
                is GameKey -> drawKey(canvas, c, l.opacity, pressed = false)
                is GameStick -> drawStick(canvas, c, l.opacity, null)
            }
            frame(canvas, c, c.id == ed.selected, c.id == ed.dragging || c.id == ed.resizing)
        }
        // 辅助线
        line.pathEffect = null
        line.strokeWidth = 1.5f * d
        line.color = ACCENT
        ed.guideX?.let { canvas.drawLine(it, geo.top, it, geo.top + geo.height, line) }
        ed.guideY?.let { canvas.drawLine(geo.left, it, geo.left + geo.width, it, line) }
    }

    /** 编辑时按钮外面的框：虚线；选中的是主题色的实线，拖着的再粗一点 */
    private fun frame(canvas: Canvas, c: GameControl, selected: Boolean, dragging: Boolean) {
        val geo = view.game.geo
        val r = geo.radius(c) + 4 * d
        line.strokeWidth = (if (dragging) 3f else if (selected) 2.5f else 1.2f) * d
        line.pathEffect = if (selected) null else dash
        line.color = if (selected) ACCENT else Color.argb(200, 255, 255, 255)
        canvas.drawCircle(geo.cx(c), geo.cy(c), r, line)
        if (selected) {
            fill.color = ACCENT
            fill.alpha = 50
            canvas.drawCircle(geo.cx(c), geo.cy(c), r, fill)
        }
    }

    private fun drawKey(canvas: Canvas, k: GameKey, opacity: Float, pressed: Boolean) {
        val geo = view.game.geo
        val cx = geo.cx(k)
        val cy = geo.cy(k)
        val r0 = geo.radius(k)
        val r = if (pressed) r0 * 0.94f else r0
        val a = opacity
        // 底
        if (pressed) {
            fill.color = ACCENT
            fill.alpha = (255 * max(a, 0.55f) * 0.9f).toInt()
        } else {
            fill.color = Color.BLACK
            fill.alpha = (255 * a * 0.45f).toInt()
        }
        canvas.drawCircle(cx, cy, r, fill)
        // 边
        line.pathEffect = null
        line.strokeWidth = 1.5f * d
        line.color = Color.WHITE
        line.alpha = (255 * a * (if (pressed) 0.9f else 0.6f)).toInt()
        canvas.drawCircle(cx, cy, r, line)
        // 切换着：外面再一圈主题色，手离开了也看得出它还按着
        if (k.mode == KeyMode.TOGGLE && pressed) {
            line.color = ACCENT
            line.alpha = 255
            line.strokeWidth = 3 * d
            canvas.drawCircle(cx, cy, r + 3 * d, line)
        }
        // 字：太长就缩小，放得进圆里
        val label = GameKeys.caption(k, str)
        text.textSize = r0 * when {
            label.length <= 1 -> 0.72f
            label.length <= 3 -> 0.5f
            else -> 0.4f
        }
        val w = text.measureText(label)
        val maxW = r0 * 1.5f
        if (w > maxW) text.textSize *= maxW / w
        text.alpha = (255 * max(a, 0.45f)).toInt()
        canvas.drawText(label, cx, cy - (text.descent() + text.ascent()) / 2, text)
        // 能转视角：下面一个小准星
        if (k.look) {
            val y = cy + r0 * 0.62f
            val s = max(2.5f * d, r0 * 0.09f)
            line.color = Color.WHITE
            line.alpha = (255 * max(a, 0.45f)).toInt()
            line.strokeWidth = 1.2f * d
            canvas.drawCircle(cx, y, s, line)
            canvas.drawLine(cx - s * 1.8f, y, cx - s * 0.6f, y, line)
            canvas.drawLine(cx + s * 0.6f, y, cx + s * 1.8f, y, line)
        }
        // 点一下切换：上面一个小圆点（切换着时是实心的）
        if (k.mode == KeyMode.TOGGLE) {
            val y = cy - r0 * 0.64f
            val s = max(2.2f * d, r0 * 0.07f)
            if (pressed) {
                fill.color = Color.WHITE
                fill.alpha = 255
                canvas.drawCircle(cx, y, s, fill)
            } else {
                line.color = Color.WHITE
                line.alpha = (255 * max(a, 0.45f)).toInt()
                line.strokeWidth = 1.2f * d
                canvas.drawCircle(cx, y, s, line)
            }
        }
    }

    private fun drawStick(canvas: Canvas, s: GameStick, opacity: Float, st: GameControls.StickState?) {
        val geo = view.game.geo
        val r = geo.radius(s)
        val bx = st?.cx ?: geo.cx(s)
        val by = st?.cy ?: geo.cy(s)
        val a = opacity
        // 推出边缘就加按的键：按着时画出外圈，亮起来表示加按了
        if (st != null && s.sprint != null) {
            line.pathEffect = if (st.sprint) null else dash
            line.color = if (st.sprint) ACCENT else Color.WHITE
            line.alpha = if (st.sprint) 255 else (255 * a * 0.6f).toInt()
            line.strokeWidth = (if (st.sprint) 3f else 1.2f) * d
            canvas.drawCircle(bx, by, r * GameControls.SPRINT_IN, line)
            line.pathEffect = null
        }
        // 底盘
        fill.color = Color.BLACK
        fill.alpha = (255 * a * (if (st != null) 0.5f else 0.32f)).toInt()
        canvas.drawCircle(bx, by, r, fill)
        line.pathEffect = null
        line.strokeWidth = 1.5f * d
        line.color = Color.WHITE
        line.alpha = (255 * a * 0.6f).toInt()
        canvas.drawCircle(bx, by, r, line)
        // 四个方向的键名，按着的亮起来
        val labels = GameKeys.stickLabels(s, str)
        val offs = arrayOf(0f to -1f, -1f to 0f, 0f to 1f, 1f to 0f)
        text.textSize = r * 0.2f
        for (i in 0..3) {
            val on = st?.dirs?.get(i) == true
            val lx = bx + offs[i].first * r * 0.72f
            val ly = by + offs[i].second * r * 0.72f
            if (on) {
                fill.color = ACCENT
                fill.alpha = 230
                canvas.drawCircle(lx, ly, r * 0.2f, fill)
            }
            text.alpha = if (on) 255 else (255 * max(a, 0.45f) * 0.85f).toInt()
            canvas.drawText(labels[i], lx, ly - (text.descent() + text.ascent()) / 2, text)
        }
        // 摇杆头
        val kx = st?.kx ?: bx
        val ky = st?.ky ?: by
        fill.color = Color.WHITE
        fill.alpha = (255 * max(a, 0.4f) * (if (st != null) 0.85f else 0.55f)).toInt()
        canvas.drawCircle(kx, ky, r * 0.36f, fill)
    }

    companion object {
        /** 主题色（[app.airhand.android.ui.theme.Brand.accent]） */
        private val ACCENT = Color.rgb(0x5B, 0x7C, 0xFF)
    }
}

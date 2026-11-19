package app.airhand.android.ui.session

import app.airhand.android.data.GameKey
import app.airhand.android.data.GameLayout
import app.airhand.android.data.GameStick
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import kotlin.math.hypot

/** 编辑方案：点选、拖动、对齐、撤销、添加 */
class GameEditorTest {
    /** 区域 840×390，1dp = 1px */
    private val geo = GameGeometry(1f).apply { set(0f, 0f, 840f, 390f) }

    // A 在 (168, 195)，B 在 (504, 117)，半径都是 30
    private val a = GameKey(1, 0.2f, 0.5f, 60f, listOf("KeyA"))
    private val b = GameKey(2, 0.6f, 0.3f, 60f, listOf("KeyB"))
    private val original = GameLayout("l", "test", listOf(a, b))
    private var ticks = 0
    private val ed = LayoutEditor(original, geo, slop = 8f, redraw = {}, tick = { ticks++ })

    private fun cx(id: Int) = geo.cx(ed.layout.control(id)!!)
    private fun cy(id: Int) = geo.cy(ed.layout.control(id)!!)

    @Test
    fun tapSelectsAndTapOnEmptySpaceDeselects() {
        ed.down(0, 170f, 196f)
        ed.up(0)
        assertEquals(a.id, ed.selected)
        assertFalse("点一下不算改动", ed.dirty)
        assertFalse(ed.canUndo)
        ed.settings = true
        ed.down(0, 400f, 300f)
        ed.up(0)
        assertNull(ed.selected)
        assertFalse(ed.settings)
    }

    @Test
    fun dragMovesAndIsOneUndoStep() {
        ed.down(0, 168f, 195f)
        ed.move(0, 172f, 195f) // 还没过 slop：不动
        assertEquals(168f, cx(a.id), 0.01f)
        ed.move(0, 230f, 250f)
        ed.move(0, 268f, 255f)
        ed.up(0)
        assertEquals(268f, cx(a.id), 0.01f)
        assertEquals(255f, cy(a.id), 0.01f)
        assertTrue(ed.dirty)
        assertTrue(ed.canUndo)
        ed.undo()
        assertEquals(original, ed.layout)
        assertFalse(ed.canUndo)
    }

    @Test
    fun grabbingOffCenterDoesNotJump() {
        // 按在按钮边上拖：按钮跟着手指平移，不是中心跳到手指下面
        ed.down(0, 188f, 195f)
        ed.move(0, 288f, 255f)
        assertEquals(268f, cx(a.id), 0.01f)
        assertEquals(255f, cy(a.id), 0.01f)
    }

    @Test
    fun dragSnapsToOtherButtonsAndTheMiddle() {
        ed.down(0, 168f, 195f)
        ed.move(0, 300f, 121f) // y 离 B 的中心 4px：吸过去
        assertEquals(117f, cy(a.id), 0.01f)
        assertEquals(117f, ed.guideY!!, 0.01f)
        assertNull(ed.guideX)
        assertEquals(1, ticks)
        ed.move(0, 416f, 160f) // x 离区域中线 4px
        assertEquals(420f, cx(a.id), 0.01f)
        assertEquals(420f, ed.guideX!!, 0.01f)
        assertNull(ed.guideY)
        ed.up(0)
        assertNull(ed.guideX)
        assertNull(ed.dragging)
    }

    @Test
    fun buttonsStayInsideTheArea() {
        ed.down(0, 168f, 195f)
        ed.move(0, -100f, 500f)
        ed.up(0)
        assertEquals(30f, cx(a.id), 0.01f)
        assertEquals(360f, cy(a.id), 0.01f)
        // 变大了也拉回来
        ed.update(a.id) { it.sized(120f) }
        assertEquals(60f, cx(a.id), 0.01f)
        assertEquals(330f, cy(a.id), 0.01f)
    }

    @Test
    fun addedButtonsGoToAFreeSpotAndAreSelected() {
        repeat(5) { ed.add { id -> GameKey(id, 0.5f, 0.5f, 60f, listOf("KeyX")) } }
        ed.add { id -> GameStick(id, 0.5f, 0.5f) }
        val cs = ed.layout.controls
        assertEquals(8, cs.size)
        assertEquals(cs.last().id, ed.selected)
        assertEquals(cs.size, cs.map { it.id }.toSet().size)
        for (i in cs.indices) for (j in i + 1 until cs.size) {
            val d = hypot(geo.cx(cs[i]) - geo.cx(cs[j]), geo.cy(cs[i]) - geo.cy(cs[j]))
            assertTrue("${cs[i]} 和 ${cs[j]} 挤在一起", d >= geo.radius(cs[i]) + geo.radius(cs[j]))
        }
    }

    @Test
    fun sliderDragIsOneUndoStep() {
        ed.snapshot()
        for (s in listOf(70f, 80f, 90f)) ed.update(a.id, record = false) { it.sized(s) }
        assertEquals(90f, ed.layout.control(a.id)!!.size, 0.01f)
        ed.undo()
        assertEquals(original, ed.layout)
    }

    @Test
    fun duplicateAndDelete() {
        ed.duplicate(a.id)
        val copy = ed.layout.controls.last()
        assertNotEquals(a.id, copy.id)
        assertEquals(copy.id, ed.selected)
        assertEquals(a.copy(id = copy.id, x = copy.x, y = copy.y), copy)
        ed.delete(copy.id)
        assertNull(ed.selected)
        assertEquals(original, ed.layout)
        ed.undo()
        assertEquals(3, ed.layout.controls.size)
        // 撤销到选中的按钮不存在了：取消选中
        ed.selected = copy.id
        ed.undo()
        assertNull(ed.selected)
    }

    @Test
    fun pinchResizesTheSelectedButtonInOneUndoStep() {
        ed.down(0, 168f, 195f)
        ed.up(0)
        // 在空白处捏合：调的是选中的 A，中心不动
        ed.down(0, 400f, 300f)
        ed.down(1, 500f, 300f)
        assertEquals(a.id, ed.resizing)
        ed.move(1, 550f, 300f)
        ed.move(0, 400f, 300f)
        assertEquals(90f, ed.layout.control(a.id)!!.size, 0.01f)
        assertEquals(168f, cx(a.id), 0.01f)
        // 不会超过上限
        ed.move(1, 900f, 300f)
        assertEquals(GameKey.MAX, ed.layout.control(a.id)!!.size, 0.01f)
        ed.up(1)
        assertNull(ed.resizing)
        // 剩下的手指不再拖动，抬起也不取消选中
        ed.move(0, 100f, 100f)
        ed.up(0)
        assertEquals(168f, cx(a.id), 0.01f)
        assertEquals(a.id, ed.selected)
        ed.undo()
        assertEquals(original, ed.layout)
    }

    @Test
    fun pinchWhileDraggingResizesTheDraggedButton() {
        ed.down(0, 504f, 117f)
        ed.move(0, 524f, 117f) // 拖了 20px
        ed.down(1, 624f, 117f)
        assertEquals(b.id, ed.resizing)
        ed.move(1, 574f, 117f) // 两指距离 100 → 50
        assertEquals(GameKey.MIN, ed.layout.control(b.id)!!.size, 0.01f)
        ed.up(1)
        ed.up(0)
        // 拖动和捏合各算一步
        ed.undo()
        assertEquals(60f, ed.layout.control(b.id)!!.size, 0.01f)
        assertEquals(524f, cx(b.id), 0.01f)
        ed.undo()
        assertEquals(original, ed.layout)
    }

    @Test
    fun pinchWithNothingSelectedDoesNothing() {
        ed.down(0, 400f, 300f)
        ed.down(1, 500f, 300f)
        assertNull(ed.resizing)
        ed.move(1, 700f, 300f)
        ed.up(1)
        ed.up(0)
        assertEquals(original, ed.layout)
        assertFalse(ed.canUndo)
    }
}

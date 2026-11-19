package app.airhand.android.data

import app.airhand.android.ui.session.GameGeometry
import app.airhand.android.ui.session.GameKeys
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import kotlin.math.hypot

/** 游戏按键方案：存取、容错、模板摆得下 */
class GameLayoutsTest {
    private val sample = GameLayout(
        id = "abc",
        name = "射击",
        controls = listOf(
            GameStick(1, 0.15f, 0.68f, 150f, GameBind.WASD, sprint = "ShiftLeft", floating = false),
            GameKey(2, 0.84f, 0.72f, 84f, listOf(GameBind.mouse("left")), label = "开火", look = true),
            GameKey(3, 0.5f, 0.1f, 44f, listOf("ControlLeft", "Digit1"), mode = KeyMode.TOGGLE),
            GameKey(4, 0.9f, 0.2f, 50f, listOf(GameBind.WHEEL_DOWN)),
        ),
        opacity = 0.4f,
        look = 1.5f,
        haptics = false,
    )

    @Test
    fun layoutRoundTrips() {
        assertEquals(sample, GameLayout.parse(JSONObject(sample.toJson().toString())))
    }

    @Test
    fun dataRoundTripsAndKeepsDeviceOrder() {
        val d = GameData(
            listOf(sample, sample.copy(id = "def", name = "b")),
            linkedMapOf("111111" to DeviceGame(true, "def"), "222222" to DeviceGame(false, null)),
        )
        val back = GameData.parse(JSONObject(d.toJson().toString()))
        assertEquals(d, back)
        assertEquals(listOf("111111", "222222"), back.devices.keys.toList())
    }

    @Test
    fun deviceFallsBackToTheFirstLayout() {
        val d = GameData(listOf(sample, sample.copy(id = "def")), mapOf("1" to DeviceGame(true, "gone")))
        assertEquals("abc", d.layoutFor("1")?.id)
        assertEquals("abc", d.layoutFor("never")?.id)
        assertTrue(d.isOn("1"))
        assertFalse(d.isOn("never"))
        assertNull(GameData(emptyList()).layoutFor("1"))
    }

    @Test
    fun brokenOrNewerDataIsUsedAsFarAsPossible() {
        val o = JSONObject()
            .put("id", "x")
            .put("name", "n")
            .put("opacity", 5)
            .put("look", 0)
            .put(
                "controls",
                JSONArray()
                    .put(JSONObject().put("type", "key").put("id", 1).put("x", 2).put("y", -1).put("size", 500).put("binds", JSONArray().put("KeyQ").put("bad key").put("mouse:nope")))
                    .put(JSONObject().put("type", "key").put("id", 2).put("binds", JSONArray().put("")))
                    .put(JSONObject().put("type", "gyro").put("id", 3))
                    .put(JSONObject().put("type", "stick").put("id", 4).put("keys", JSONArray().put("KeyW")).put("sprint", "?!").put("size", 10))
                    .put(JSONObject().put("type", "key").put("id", 1).put("binds", JSONArray().put("KeyZ")))
                    .put(JSONObject().put("type", "key").put("binds", JSONArray().put("KeyZ"))),
            )
        val l = GameLayout.parse(o)!!
        assertEquals(1f, l.opacity, 0f)
        assertEquals(GameLayout.MIN_LOOK, l.look, 0f)
        assertEquals(listOf(1, 4), l.controls.map { it.id })
        val k = l.controls[0] as GameKey
        assertEquals(listOf("KeyQ"), k.binds)
        assertEquals(1f, k.x, 0f)
        assertEquals(0f, k.y, 0f)
        assertEquals(GameKey.MAX, k.size, 0f)
        val s = l.controls[1] as GameStick
        assertEquals(GameBind.WASD, s.keys)
        assertNull(s.sprint)
        assertEquals(GameStick.MIN, s.size, 0f)
        assertNull(GameLayout.parse(JSONObject().put("name", "no id")))
    }

    @Test
    fun bindTokens() {
        assertTrue(GameBind.valid("KeyW"))
        assertTrue(GameBind.valid("mouse:left"))
        assertTrue(GameBind.valid("wheel:up"))
        assertFalse(GameBind.valid("mouse:nope"))
        assertFalse(GameBind.valid(""))
        assertFalse(GameBind.valid("Key W"))
        assertEquals("right", GameBind.mouseButton("mouse:right"))
        assertNull(GameBind.mouseButton("KeyR"))
        assertEquals(120, GameBind.wheel(GameBind.WHEEL_UP))
        assertEquals(-120, GameBind.wheel(GameBind.WHEEL_DOWN))
        assertEquals(0, GameBind.wheel("KeyW"))
    }

    /**
     * 模板在横屏手机（约 840×390 dp）上摆得下：都在屏幕里、互相不挤在一起、
     * 不压着菜单圆钮默认的位置（右边缘、35% 高处，见 PhoneSession.MenuHandle）
     */
    @Test
    fun templatesFitALandscapePhone() {
        val geo = GameGeometry(1f).apply { set(0f, 0f, 840f, 390f) }
        val handleX = 840f - 8f - 24f
        val handleY = 0.35f * (390f - 48f) + 24f
        for (t in GameTemplate.entries) {
            val cs = t.controls()
            assertEquals("$t: id 不重复", cs.size, cs.map { it.id }.toSet().size)
            for (c in cs) {
                val r = geo.radius(c)
                assertTrue("$t $c 出界", geo.cx(c) - r >= 0 && geo.cx(c) + r <= 840 && geo.cy(c) - r >= 0 && geo.cy(c) + r <= 390)
                assertTrue("$t $c 压着菜单圆钮", hypot(geo.cx(c) - handleX, geo.cy(c) - handleY) >= r + 24f)
                when (c) {
                    is GameKey -> assertTrue("$t $c", c.binds.isNotEmpty() && c.binds.all(GameBind::valid))
                    is GameStick -> assertTrue("$t $c", c.keys.size == 4 && c.keys.all(GameBind::valid))
                }
            }
            for (i in cs.indices) for (j in i + 1 until cs.size) {
                val d = hypot(geo.cx(cs[i]) - geo.cx(cs[j]), geo.cy(cs[i]) - geo.cy(cs[j]))
                assertTrue("$t: ${cs[i]} 和 ${cs[j]} 挤在一起", d >= geo.radius(cs[i]) + geo.radius(cs[j]) + 4f)
            }
        }
    }

    /** 选键面板里的每个键都有短名字（放得进按钮），而且都是被控端认得的键 */
    @Test
    fun everyPickableKeyHasAShortLabel() {
        val text: (Int) -> String = { "Wheel ↑" }
        val all = GameKeys.groups.flatMap { it.tokens }
        assertEquals(all.size, all.toSet().size)
        for (t in all) {
            assertTrue(t, GameBind.valid(t))
            val label = GameKeys.label(t, text)
            assertTrue("$t → $label", label.isNotEmpty() && label.length <= 7)
        }
        assertEquals("Ctrl+1", GameKeys.label(listOf("ControlLeft", "Digit1"), text))
        assertEquals("N7", GameKeys.label("Numpad7", text))
        assertEquals("W", GameKeys.label("KeyW", text))
    }
}

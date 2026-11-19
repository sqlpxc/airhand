package app.airhand.android.data

import android.content.Context
import androidx.annotation.StringRes
import app.airhand.android.R
import app.airhand.android.core.optStringOrNull
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.util.UUID

/*
 * 游戏按键：控制电脑时叠在画面上的按钮和摇杆（见 app.airhand.android.ui.session.GameControls）。
 * 只在控制端，发的是协议里已有的按键、鼠标键、滚轮和相对移动，被控端不用改。
 *
 * 位置是按钮中心在可用区域（视图去掉刘海、留一点边）里的比例，大小是 dp：
 * 换一台屏幕比例不同的手机，按钮还在相对的位置，按起来一样大。
 */

/**
 * 按钮按下去发什么（一个"键"）：键盘键是 W3C `KeyboardEvent.code`（`KeyW`、`ShiftLeft`），
 * 鼠标键是 `mouse:left` 这样，滚轮是 `wheel:up` / `wheel:down`
 */
object GameBind {
    const val MOUSE = "mouse:"
    const val WHEEL_UP = "wheel:up"
    const val WHEEL_DOWN = "wheel:down"

    /** 和协议的 `MouseButton` 一样 */
    val MOUSE_BUTTONS = listOf("left", "right", "middle", "back", "forward")

    /** 摇杆的四个方向：上、左、下、右 */
    val WASD = listOf("KeyW", "KeyA", "KeyS", "KeyD")
    val ARROWS = listOf("ArrowUp", "ArrowLeft", "ArrowDown", "ArrowRight")

    /** 一个按钮最多同时按几个键（组合键） */
    const val MAX_BINDS = 3

    fun mouse(button: String) = MOUSE + button

    /** 鼠标键的名字（`left`…）；不是鼠标键时为 null */
    fun mouseButton(token: String): String? =
        if (token.startsWith(MOUSE)) token.removePrefix(MOUSE).takeIf { it in MOUSE_BUTTONS } else null

    /** 滚轮转一格（120 = 向上，和协议一样）；不是滚轮时为 0 */
    fun wheel(token: String): Int = when (token) {
        WHEEL_UP -> 120
        WHEEL_DOWN -> -120
        else -> 0
    }

    /** 认得的键：鼠标键、滚轮，或者像 W3C code 那样只有字母数字的键名（不认识的键被控端自己会跳过） */
    fun valid(token: String): Boolean =
        mouseButton(token) != null || wheel(token) != 0 ||
            (token.length in 1..32 && token.all { it in 'a'..'z' || it in 'A'..'Z' || it in '0'..'9' })
}

/** 按钮怎么按 */
enum class KeyMode(val id: String) {
    /** 按着按钮就按着键，松手就松开 */
    HOLD("hold"),

    /** 点一下按下，再点一下松开（自动奔跑、一直蹲着这类） */
    TOGGLE("toggle");

    companion object {
        fun of(id: String?) = entries.firstOrNull { it.id == id } ?: HOLD
    }
}

/** 画面上的一个按钮或摇杆。[id] 在方案里唯一 */
sealed interface GameControl {
    val id: Int

    /** 中心在区域里的位置（0..1） */
    val x: Float
    val y: Float

    /** 直径（dp） */
    val size: Float

    fun at(x: Float, y: Float): GameControl
    fun sized(size: Float): GameControl
    fun withId(id: Int): GameControl
    fun toJson(): JSONObject
}

/**
 * 按钮：按下时按下 [binds] 里的键（几个就是组合键，按顺序按下、倒序松开）。
 * [look]：按着的时候手指拖动也转视角（开火键：一边开火一边瞄准）
 */
data class GameKey(
    override val id: Int,
    override val x: Float,
    override val y: Float,
    override val size: Float = DEFAULT,
    val binds: List<String>,
    /** 按钮上的字，空字符串表示按键名（[app.airhand.android.ui.session.GameKeys.label]） */
    val label: String = "",
    val mode: KeyMode = KeyMode.HOLD,
    val look: Boolean = false,
) : GameControl {
    override fun at(x: Float, y: Float) = copy(x = x.coerceIn(0f, 1f), y = y.coerceIn(0f, 1f))
    override fun sized(size: Float) = copy(size = size.coerceIn(MIN, MAX))
    override fun withId(id: Int) = copy(id = id)

    override fun toJson(): JSONObject = JSONObject()
        .put("type", "key")
        .put("id", id)
        .put("x", x.toDouble())
        .put("y", y.toDouble())
        .put("size", size.toDouble())
        .put("binds", JSONArray(binds))
        .put("label", label)
        .put("mode", mode.id)
        .put("look", look)

    companion object {
        const val MIN = 40f
        const val MAX = 120f
        const val DEFAULT = 60f

        /** 按钮上自己写的字最多几个 */
        const val MAX_LABEL = 8
    }
}

/**
 * 摇杆：推向哪边就按着哪个方向的键，斜着推按两个（8 个方向）。[keys] 是上、左、下、右。
 * [sprint]：推出边缘再往外时加按的键（例如 Shift 奔跑），null 表示不加。
 * [floating]：中心落在手指按下的地方（不用正好按在中间）
 */
data class GameStick(
    override val id: Int,
    override val x: Float,
    override val y: Float,
    override val size: Float = DEFAULT,
    val keys: List<String> = GameBind.WASD,
    val sprint: String? = null,
    val floating: Boolean = true,
) : GameControl {
    override fun at(x: Float, y: Float) = copy(x = x.coerceIn(0f, 1f), y = y.coerceIn(0f, 1f))
    override fun sized(size: Float) = copy(size = size.coerceIn(MIN, MAX))
    override fun withId(id: Int) = copy(id = id)

    override fun toJson(): JSONObject = JSONObject()
        .put("type", "stick")
        .put("id", id)
        .put("x", x.toDouble())
        .put("y", y.toDouble())
        .put("size", size.toDouble())
        .put("keys", JSONArray(keys))
        .put("sprint", sprint ?: JSONObject.NULL)
        .put("floating", floating)

    companion object {
        const val MIN = 100f
        const val MAX = 240f
        const val DEFAULT = 150f

        /** 摇杆推出边缘时可以加按的键 */
        val SPRINT_KEYS = listOf("ShiftLeft", "ControlLeft", "AltLeft")
    }
}

/** 一套按键（"方案"），例如每个游戏一套 */
data class GameLayout(
    val id: String,
    val name: String,
    val controls: List<GameControl> = emptyList(),
    /** 按钮的不透明度 */
    val opacity: Float = DEFAULT_OPACITY,
    /** 转视角的灵敏度（倍数）：空白处拖动、可以转视角的按钮都按它 */
    val look: Float = 1f,
    /** 按下时振动 */
    val haptics: Boolean = true,
) {
    fun control(id: Int): GameControl? = controls.firstOrNull { it.id == id }
    fun nextId(): Int = (controls.maxOfOrNull { it.id } ?: 0) + 1
    fun with(c: GameControl) = copy(controls = controls.map { if (it.id == c.id) c else it })
    fun without(id: Int) = copy(controls = controls.filter { it.id != id })
    fun plus(c: GameControl) = copy(controls = controls + c)

    fun toJson(): JSONObject = JSONObject()
        .put("id", id)
        .put("name", name)
        .put("opacity", opacity.toDouble())
        .put("look", look.toDouble())
        .put("haptics", haptics)
        .put("controls", JSONArray(controls.map { it.toJson() }))

    companion object {
        const val DEFAULT_OPACITY = 0.6f
        const val MIN_OPACITY = 0.15f
        const val MIN_LOOK = 0.25f
        const val MAX_LOOK = 4f

        /** 存的东西不对（手改过、版本不同）时尽量用：坏掉的按钮跳过，数值拉回范围里；新版本才有的控件也跳过 */
        fun parse(o: JSONObject): GameLayout? {
            val id = o.optString("id").takeIf { it.isNotBlank() } ?: return null
            val list = o.optJSONArray("controls") ?: JSONArray()
            val controls = (0 until list.length())
                .mapNotNull { i -> list.optJSONObject(i)?.let(::parseControl) }
                .distinctBy { it.id }
            return GameLayout(
                id = id,
                name = o.optString("name"),
                controls = controls,
                opacity = o.float("opacity", DEFAULT_OPACITY).coerceIn(MIN_OPACITY, 1f),
                look = o.float("look", 1f).coerceIn(MIN_LOOK, MAX_LOOK),
                haptics = o.optBoolean("haptics", true),
            )
        }

        fun parseControl(o: JSONObject): GameControl? {
            val id = o.optInt("id", -1).takeIf { it >= 0 } ?: return null
            val x = o.float("x", 0.5f).coerceIn(0f, 1f)
            val y = o.float("y", 0.5f).coerceIn(0f, 1f)
            return when (o.optString("type")) {
                "key" -> {
                    val binds = o.optJSONArray("binds").strings().filter(GameBind::valid).distinct().take(GameBind.MAX_BINDS)
                    if (binds.isEmpty()) return null
                    GameKey(
                        id = id,
                        x = x,
                        y = y,
                        size = o.float("size", GameKey.DEFAULT).coerceIn(GameKey.MIN, GameKey.MAX),
                        binds = binds,
                        label = o.optString("label").take(GameKey.MAX_LABEL),
                        mode = KeyMode.of(o.optStringOrNull("mode")),
                        look = o.optBoolean("look", false),
                    )
                }
                "stick" -> {
                    val keys = o.optJSONArray("keys").strings()
                    GameStick(
                        id = id,
                        x = x,
                        y = y,
                        size = o.float("size", GameStick.DEFAULT).coerceIn(GameStick.MIN, GameStick.MAX),
                        keys = if (keys.size == 4 && keys.all(GameBind::valid)) keys else GameBind.WASD,
                        sprint = o.optStringOrNull("sprint")?.takeIf(GameBind::valid),
                        floating = o.optBoolean("floating", true),
                    )
                }
                else -> null
            }
        }
    }
}

private fun JSONObject.float(key: String, default: Float): Float =
    optDouble(key, default.toDouble()).toFloat().takeIf { it.isFinite() } ?: default

private fun JSONArray?.strings(): List<String> =
    if (this == null) emptyList() else (0 until length()).mapNotNull { optString(it).takeIf { s -> s.isNotEmpty() } }

/**
 * 新建方案时的模板。位置按横屏手机（约 840×390 dp）排好：左手摇杆、右手一圈按钮，
 * 右边中间留给转视角的手指，右边缘偏上留给菜单圆钮
 */
enum class GameTemplate(@StringRes val title: Int, @StringRes val desc: Int) {
    SHOOTER(R.string.game_tpl_shooter, R.string.game_tpl_shooter_desc),
    SKILLS(R.string.game_tpl_skills, R.string.game_tpl_skills_desc),
    ARROWS(R.string.game_tpl_arrows, R.string.game_tpl_arrows_desc),
    BLANK(R.string.game_tpl_blank, R.string.game_tpl_blank_desc);

    fun controls(): List<GameControl> {
        val out = ArrayList<GameControl>()
        fun key(x: Float, y: Float, size: Float, vararg binds: String, look: Boolean = false) {
            out += GameKey(out.size + 1, x, y, size, binds.toList(), look = look)
        }

        fun stick(x: Float, y: Float, keys: List<String>, sprint: String?) {
            out += GameStick(out.size + 1, x, y, GameStick.DEFAULT, keys, sprint)
        }
        when (this) {
            SHOOTER -> {
                stick(0.15f, 0.68f, GameBind.WASD, "ShiftLeft")
                key(0.84f, 0.72f, 84f, GameBind.mouse("left"), look = true) // 开火：按着拖动就是瞄准
                key(0.84f, 0.46f, 56f, GameBind.mouse("right")) // 瞄准镜
                key(0.95f, 0.56f, 60f, "Space")
                key(0.95f, 0.84f, 52f, "KeyC")
                key(0.71f, 0.84f, 52f, "KeyR")
                key(0.30f, 0.80f, 52f, "KeyE")
                key(0.28f, 0.58f, 52f, "KeyQ")
                key(0.42f, 0.10f, 44f, "Digit1")
                key(0.50f, 0.10f, 44f, "Digit2")
                key(0.58f, 0.10f, 44f, "Digit3")
                key(0.07f, 0.12f, 44f, "Tab")
                key(0.17f, 0.12f, 44f, "Escape")
            }
            SKILLS -> {
                key(0.60f, 0.86f, 60f, "KeyQ")
                key(0.70f, 0.86f, 60f, "KeyW")
                key(0.80f, 0.86f, 60f, "KeyE")
                key(0.90f, 0.86f, 60f, "KeyR")
                key(0.80f, 0.66f, 52f, "KeyD")
                key(0.90f, 0.66f, 52f, "KeyF")
                for (i in 1..4) key(0.06f + 0.07f * (i - 1), 0.86f, 48f, "Digit$i")
                key(0.06f, 0.66f, 48f, "KeyB")
                key(0.13f, 0.66f, 48f, "Space")
                key(0.06f, 0.12f, 44f, "Tab")
                key(0.13f, 0.12f, 44f, "Escape")
            }
            ARROWS -> {
                stick(0.15f, 0.68f, GameBind.ARROWS, null)
                key(0.80f, 0.80f, 64f, "KeyZ")
                key(0.91f, 0.62f, 64f, "KeyX")
                key(0.69f, 0.62f, 56f, "KeyC")
                key(0.80f, 0.44f, 56f, "Space")
                key(0.44f, 0.10f, 48f, "Escape")
                key(0.56f, 0.10f, 48f, "Enter")
            }
            BLANK -> {}
        }
        return out
    }

    fun build(id: String, name: String) = GameLayout(id, name, controls())
}

/** 某台设备上开没开游戏按键、用哪个方案（按控制码记） */
data class DeviceGame(val on: Boolean = false, val layout: String? = null)

data class GameData(val layouts: List<GameLayout>, val devices: Map<String, DeviceGame> = emptyMap()) {
    /** 这台设备用的方案：选过的；没选过或者删掉了就用第一个 */
    fun layoutFor(code: String): GameLayout? =
        devices[code]?.layout?.let { id -> layouts.firstOrNull { it.id == id } } ?: layouts.firstOrNull()

    fun isOn(code: String): Boolean = devices[code]?.on == true

    fun toJson(): JSONObject = JSONObject()
        .put("layouts", JSONArray(layouts.map { it.toJson() }))
        // 数组：保留先后（最近用过的在后面，超过上限时丢掉最前面的）
        .put(
            "devices",
            JSONArray(devices.map { (code, d) -> JSONObject().put("code", code).put("on", d.on).put("layout", d.layout ?: JSONObject.NULL) }),
        )

    companion object {
        /** 记住多少台设备的选择 */
        const val MAX_DEVICES = 100

        fun parse(o: JSONObject): GameData {
            val ls = o.optJSONArray("layouts") ?: JSONArray()
            val ds = o.optJSONArray("devices") ?: JSONArray()
            val devices = LinkedHashMap<String, DeviceGame>()
            for (i in 0 until ds.length()) {
                val d = ds.optJSONObject(i) ?: continue
                val code = d.optString("code").takeIf { it.isNotBlank() } ?: continue
                devices[code] = DeviceGame(d.optBoolean("on"), d.optStringOrNull("layout"))
            }
            return GameData(
                layouts = (0 until ls.length()).mapNotNull { i -> ls.optJSONObject(i)?.let(GameLayout::parse) }.distinctBy { it.id },
                devices = devices,
            )
        }
    }
}

/** 游戏按键的方案（`game.json`）。第一次用时有射击、技能、方向键三套，可以改、删、新建 */
class GameStore(private val context: Context) {
    private val file = File(context.filesDir, "game.json")
    private val _flow = MutableStateFlow(load())
    val flow: StateFlow<GameData> = _flow.asStateFlow()
    val value: GameData get() = _flow.value

    private fun load(): GameData =
        JsonFile.read(file)?.let { runCatching { GameData.parse(JSONObject(it)) }.getOrNull() }
            ?: GameData(listOf(GameTemplate.SHOOTER, GameTemplate.SKILLS, GameTemplate.ARROWS).map { it.build(newId(), context.getString(it.title)) })

    @Synchronized
    private fun update(f: (GameData) -> GameData) {
        val next = f(_flow.value)
        if (next == _flow.value) return
        _flow.value = next
        JsonFile.write(file, next.toJson().toString())
    }

    /** 保存（新的加在最后） */
    fun save(l: GameLayout) = update { d ->
        d.copy(layouts = if (d.layouts.any { it.id == l.id }) d.layouts.map { if (it.id == l.id) l else it } else d.layouts + l)
    }

    fun delete(id: String) = update { d -> d.copy(layouts = d.layouts.filter { it.id != id }) }

    fun rename(id: String, name: String) = update { d ->
        d.copy(layouts = d.layouts.map { if (it.id == id) it.copy(name = name.trim()) else it })
    }

    /** 从模板新建一个方案（名字重了加编号） */
    fun create(t: GameTemplate): GameLayout {
        val l = t.build(newId(), uniqueName(context.getString(t.title)))
        save(l)
        return l
    }

    fun duplicate(l: GameLayout): GameLayout {
        val c = l.copy(id = newId(), name = uniqueName(context.getString(R.string.game_copy_name, l.name)))
        save(c)
        return c
    }

    /** 记下这台设备开没开、用哪个方案（`null` 表示不变） */
    fun setDevice(code: String, on: Boolean? = null, layout: String? = null) = update { d ->
        val old = d.devices[code] ?: DeviceGame()
        val now = DeviceGame(on ?: old.on, layout ?: old.layout)
        val devices = LinkedHashMap(d.devices)
        devices.remove(code)
        devices[code] = now
        while (devices.size > GameData.MAX_DEVICES) devices.remove(devices.keys.first())
        d.copy(devices = devices)
    }

    private fun uniqueName(base: String): String {
        val names = value.layouts.map { it.name }.toSet()
        if (base !in names) return base
        return (2..999).map { "$base $it" }.firstOrNull { it !in names } ?: base
    }

    private fun newId(): String = UUID.randomUUID().toString().replace("-", "").take(12)
}

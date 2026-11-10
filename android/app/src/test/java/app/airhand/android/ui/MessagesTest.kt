package app.airhand.android.ui

import app.airhand.android.R
import app.airhand.android.core.Codec
import app.airhand.android.core.Notice
import app.airhand.android.core.OsKind
import app.airhand.android.core.Problem
import app.airhand.android.core.SystemAction
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.w3c.dom.Element
import java.io.File
import javax.xml.parsers.DocumentBuilderFactory

/**
 * 文案：Rust 端每一种问题 / 提示在 Android 上都有文字（Rust 只传类型，文字全在界面这边），
 * 中英文的词条和参数一致。与桌面控制台的 client/tests/i18n.test.ts 是同一个检查。
 */
class MessagesTest {
    /** 仓库根目录（build.gradle.kts 传进来；直接在 android/app 下运行时退回相对路径） */
    private val repo = File(System.getProperty("airhand.repo") ?: "../..")

    private data class Variant(val kind: String, val hasDetail: Boolean)

    /** 从 Rust 源码里取出某个枚举的变体（名字转成 serde 的 camelCase），以及它有没有 `detail` 字段 */
    private fun rustVariants(path: String, enumName: String): List<Variant> {
        val file = File(repo, path)
        assertTrue("找不到 ${file.absolutePath}", file.isFile)
        val body = file.readText().replace("\r\n", "\n").substringAfter("pub enum $enumName {", "").substringBefore("\n}")
        assertTrue("$path 里没有找到 enum $enumName", body.isNotBlank())
        val start = Regex("^ {4}([A-Z]\\w*)(.*)$")
        val found = ArrayList<Pair<String, StringBuilder>>()
        for (line in body.lines()) {
            val m = start.find(line)
            when {
                m != null -> found += m.groupValues[1] to StringBuilder(m.groupValues[2])
                // 多行的变体：字段在下一层缩进里（注释不算）
                line.startsWith("        ") && !line.trimStart().startsWith("//") -> found.lastOrNull()?.second?.append(line)
                line == "    }," -> {}
            }
        }
        return found.map { (name, rest) ->
            Variant(name[0].lowercaseChar() + name.substring(1), Regex("\\bdetail\\s*:").containsMatchIn(rest))
        }
    }

    private val problems by lazy { rustVariants("crates/airhand-link/src/problem.rs", "Problem") }
    private val notices by lazy { rustVariants("crates/airhand-link/src/controller/session.rs", "NoticeView") }

    // ───────────── strings.xml ─────────────

    private class Strings(val strings: Map<String, String>, val plurals: Map<String, List<String>>)

    private fun load(dir: String): Strings {
        val file = File(repo, "android/app/src/main/res/$dir/strings.xml")
        assertTrue("找不到 ${file.absolutePath}", file.isFile)
        val doc = DocumentBuilderFactory.newInstance().newDocumentBuilder().parse(file)
        fun elements(tag: String) = doc.getElementsByTagName(tag).let { l -> (0 until l.length).map { l.item(it) as Element } }
        val strings = elements("string")
            .filter { it.getAttribute("translatable") != "false" }
            .associate { it.getAttribute("name") to it.textContent }
        val plurals = elements("plurals").associate { p ->
            val items = p.getElementsByTagName("item")
            p.getAttribute("name") to (0 until items.length).map { items.item(it).textContent }
        }
        return Strings(strings, plurals)
    }

    private val en by lazy { load("values") }
    private val zh by lazy { load("values-zh") }

    /** 格式参数，例如 `%1$s`、`%2$d` */
    private fun placeholders(s: String): List<String> = Regex("%(\\d+\\$)?[sdf]").findAll(s).map { it.value }.sorted().toList()

    /** R.string 里的资源 id → 名字 */
    private val names: Map<Int, String> by lazy {
        R.string::class.java.fields.associate { it.getInt(null) to it.name }
    }

    /** 这个文案在两种语言里都存在，并且（需要时）用 `%1$s` 显示 detail */
    private fun checkText(res: Int, what: String, showsDetail: Boolean) {
        val name = names[res]
        assertNotNull("$what 的资源 id $res 不在 R.string 里", name)
        for ((lang, s) in listOf("en" to en, "zh" to zh)) {
            val text = s.strings[name]
            assertNotNull("$lang 缺少 $what 的文案 $name", text)
            if (showsDetail) assertTrue("$lang 的 $name 没有用 %1\$s 显示 $what 的 detail: $text", text!!.contains("%1\$s"))
        }
    }

    // ───────────── 测试 ─────────────

    @Test
    fun rustEnumsParse() {
        val kinds = problems.map { it.kind }
        assertTrue("Problem 解析结果不对: $kinds", kinds.size >= 15 && "certMismatch" in kinds && "incompatible" in kinds)
        // 只检查解析对不对（新增带 detail 的变体不需要改这里）
        val withDetail = problems.filter { it.hasDetail }.map { it.kind }
        assertTrue("带 detail 的 Problem 解析结果不对: $withDetail", withDetail.containsAll(listOf("serverUnreachable", "serverLost", "failed")))
        assertTrue("带 detail 的 Problem 解析结果不对: $withDetail", "certMismatch" !in withDetail && "incompatible" !in withDetail)
        // 提示的种类变了：除了补文案，还要检查 RemoteSession.handleEvent 对提示的处理（xxxRecovered 撤下 xxxFailed）
        assertEquals(
            listOf("noDisplays", "wayland", "captureFailed", "captureRecovered", "audioFailed", "audioRecovered", "missingPermissions"),
            notices.map { it.kind },
        )
        assertEquals(listOf("captureFailed", "audioFailed"), notices.filter { it.hasDetail }.map { it.kind })
    }

    @Test
    fun everyRustProblemHasText() {
        for (v in problems) {
            val res = Messages.problemRes(Problem(v.kind, detail = "D", server = 3, client = 2))
            assertNotNull("Messages.problemRes 没有处理 Rust 的 Problem::${v.kind}（会显示成通用的\"出错了\"）", res)
            checkText(res!!, "Problem::${v.kind}", v.hasDetail)
        }
        // 版本不一致：按版本号提示更新哪一方
        val newerServer = Messages.problemRes(Problem("incompatible", server = 3, client = 2))
        val olderServer = Messages.problemRes(Problem("incompatible", server = 1, client = 2))
        assertNotNull(olderServer)
        assertNotEquals(newerServer, olderServer)
        checkText(olderServer!!, "Problem::incompatible", false)
        // 不认识的类型（例如更新的 Rust 端）：显示通用的"出错了"
        assertNull(Messages.problemRes(Problem("somethingNew")))
    }

    @Test
    fun everyRustNoticeHasText() {
        for (v in notices) {
            if (v.kind.endsWith("Recovered")) {
                // 只用来撤下对应的提示，本身不显示
                assertTrue("${v.kind} 没有对应的 Failed 提示", notices.any { it.kind == v.kind.removeSuffix("Recovered") + "Failed" })
                assertNull("${v.kind} 不应该显示", Messages.noticeRes(Notice(v.kind), OsKind.WINDOWS))
                continue
            }
            for (os in OsKind.entries) {
                val variants = if (v.kind == "missingPermissions") {
                    listOf(true to true, true to false, false to true).map { (screen, input) -> Notice(v.kind, screen = screen, input = input) }
                } else {
                    listOf(Notice(v.kind, detail = "D"))
                }
                for (n in variants) {
                    val res = Messages.noticeRes(n, os)
                    assertNotNull("Messages.noticeRes 没有处理 Rust 的 NoticeView::${v.kind}（$os）", res)
                    checkText(res!!, "NoticeView::${v.kind}", v.hasDetail)
                }
            }
        }
        // Mac 缺少的是哪一项权限，三种说法各不相同；对方是手机时说的是"远程操作"
        val mac = listOf(true to true, true to false, false to true).map { (s, i) -> Messages.noticeRes(Notice("missingPermissions", screen = s, input = i), OsKind.MAC) }
        assertEquals(3, mac.toSet().size)
        assertEquals(R.string.notice_android_no_input, Messages.noticeRes(Notice("missingPermissions", input = true), OsKind.ANDROID))
    }

    /** 电视只看不操作：对方没给操作权限不用提示；缺屏幕录制权限时只说看不到画面 */
    @Test
    fun viewerSkipsInputPermissionNotices() {
        assertNull(Messages.forViewer(Notice("missingPermissions", screen = false, input = true)))
        assertNull(Messages.forViewer(Notice("missingPermissions", screen = false, input = false)))
        val both = Messages.forViewer(Notice("missingPermissions", screen = true, input = true))
        assertEquals(Notice("missingPermissions", screen = true, input = false), both)
        assertEquals(R.string.notice_mac_no_screen, Messages.noticeRes(both!!, OsKind.MAC))
        val failed = Notice("captureFailed", detail = "D")
        assertEquals(failed, Messages.forViewer(failed))
    }

    /** 从 Rust 源码里取出一个开放枚举（`open_enum!`）在 JSON 里的名字：`Back = 0 => "back",` 里的 "back" */
    private fun rustOpenEnumNames(path: String, enumName: String): List<String> {
        val file = File(repo, path)
        assertTrue("找不到 ${file.absolutePath}", file.isFile)
        val body = file.readText().replace("\r\n", "\n").substringAfter("pub enum $enumName {", "").substringBefore("\n    }")
        assertTrue("$path 里没有找到 enum $enumName", body.isNotBlank())
        return Regex("^\\s+[A-Z]\\w* = \\d+ => \"(\\w+)\",", RegexOption.MULTILINE).findAll(body).map { it.groupValues[1] }.toList()
    }

    /** Kotlin 这边的 SystemAction / OsKind / Codec 和 Rust 的枚举一一对应（JSON 里的名字是 camelCase），每个系统操作都有文案 */
    @Test
    fun systemActionsAndPlatformsMatchRust() {
        val proto = "crates/airhand-proto/src/session.rs"
        assertEquals(
            "SystemAction 和 Rust 不一致（Rust 新增的操作要在 core/Model.kt 和 actionLabel 里补上）",
            rustOpenEnumNames(proto, "SystemAction"),
            SystemAction.entries.map { it.wire },
        )
        assertEquals(
            "Platform 和 Rust 不一致（core/Model.kt 的 OsKind）",
            rustOpenEnumNames(proto, "Platform"),
            OsKind.entries.filter { it != OsKind.UNKNOWN }.map { it.wire },
        )
        assertEquals(
            "Codec 和 Rust 不一致（core/Model.kt）",
            rustOpenEnumNames(proto, "Codec"),
            Codec.entries.map { it.wire },
        )
        for (a in SystemAction.entries) {
            val name = "action_" + a.name.lowercase()
            for ((lang, s) in listOf("en" to en, "zh" to zh)) {
                assertNotNull("$lang 缺少系统操作 ${a.wire} 的文案 $name", s.strings[name])
            }
        }
    }

    @Test
    fun bothLanguagesHaveTheSameStrings() {
        assertTrue("没有读到词条", en.strings.size > 100)
        assertEquals("中文缺少的词条", emptySet<String>(), en.strings.keys - zh.strings.keys)
        assertEquals("英文（默认）缺少的词条", emptySet<String>(), zh.strings.keys - en.strings.keys)
        for ((name, text) in en.strings) {
            assertEquals("$name 的参数在两种语言里不一致", placeholders(text), placeholders(zh.strings.getValue(name)))
            assertTrue("en $name 为空", text.isNotBlank())
            assertTrue("zh $name 为空", zh.strings.getValue(name).isNotBlank())
        }
        assertEquals("单复数词条不一致", en.plurals.keys, zh.plurals.keys)
        for ((name, items) in en.plurals) {
            val enArgs = items.flatMap { placeholders(it) }.toSet()
            val zhArgs = zh.plurals.getValue(name).flatMap { placeholders(it) }.toSet()
            assertEquals("$name 的参数在两种语言里不一致", enArgs, zhArgs)
        }
    }
}

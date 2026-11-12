package app.airhand.android.ui.session

import android.view.KeyEvent as AndroidKeyEvent
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.focusable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.AspectRatio
import androidx.compose.material.icons.outlined.VideoSettings
import androidx.compose.material.icons.outlined.HighQuality
import androidx.compose.material.icons.outlined.Sync
import androidx.compose.material.icons.outlined.Info
import androidx.compose.material.icons.outlined.LinkOff
import androidx.compose.material.icons.outlined.Monitor
import androidx.compose.material.icons.outlined.Speed
import androidx.compose.material.icons.automirrored.outlined.VolumeOff
import androidx.compose.material.icons.automirrored.outlined.VolumeUp
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.input.key.KeyEventType
import androidx.compose.ui.input.key.nativeKeyCode
import androidx.compose.ui.input.key.onPreviewKeyEvent
import androidx.compose.ui.input.key.type
import androidx.compose.ui.input.key.key
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import app.airhand.android.AirhandApp
import app.airhand.android.R
import app.airhand.android.controller.RemoteSession
import app.airhand.android.core.Code
import app.airhand.android.core.Codec
import app.airhand.android.core.OsKind
import app.airhand.android.core.Preset
import app.airhand.android.core.VideoPriority
import app.airhand.android.data.ScaleMode
import app.airhand.android.ui.Messages
import app.airhand.android.ui.phone.presetName
import app.airhand.android.ui.phone.priorityName
import app.airhand.android.ui.theme.Brand
import kotlinx.coroutines.delay
import kotlin.math.roundToInt

/**
 * 电视上的远程画面：只看，不操作。遥控器：
 *
 * * 确认键 / 菜单键：打开右侧菜单（声音、音画同步、显示器、画质、编码格式、画面缩放、信息、断开）
 * * 左 / 右：切换对方的显示器（有多个时）
 * * 返回：关菜单；没开菜单时确认断开
 */
@Composable
fun TvSession(session: RemoteSession, view: RemoteView, onExit: () -> Unit) {
    val ctx = LocalContext.current
    val video by session.videoSize.collectAsState()
    val phase by session.phase.collectAsState()
    val displays by session.displays.collectAsState()
    val current by session.currentDisplay.collectAsState()
    val name by session.hostName.collectAsState()
    val notices by session.notices.collectAsState()
    val hello by session.hello.collectAsState()
    val os = OsKind.of(hello?.os)
    val settings by AirhandApp.instance.settings.flow.collectAsState()

    var menu by remember { mutableStateOf(false) }
    var info by remember { mutableStateOf(false) }
    var hint by remember { mutableStateOf(true) }
    var confirmExit by remember { mutableStateOf(false) }
    var toast by remember { mutableStateOf<String?>(null) }
    val root = remember { FocusRequester() }

    LaunchedEffect(video) { video?.let { view.setVideoSize(it.first, it.second) } }
    LaunchedEffect(settings.tvScale) { view.scaleMode = settings.tvScale }
    LaunchedEffect(phase, video) {
        if (phase == RemoteSession.Phase.Live && video != null) {
            hint = true
            delay(6000)
            hint = false
        }
    }
    LaunchedEffect(toast) {
        if (toast != null) {
            delay(2500)
            toast = null
        }
    }
    LaunchedEffect(menu, confirmExit) { if (!menu && !confirmExit) runCatching { root.requestFocus() } }

    fun switchDisplay(step: Int) {
        if (displays.size < 2) return
        val i = (displays.indexOfFirst { it.index == current }.coerceAtLeast(0) + step + displays.size) % displays.size
        session.selectDisplay(displays[i].index)
        toast = ctx.getString(R.string.display_n, displays[i].index + 1)
    }

    BackHandler {
        if (menu) menu = false else confirmExit = true
    }

    Box(
        Modifier
            .fillMaxSize()
            .background(Color.Black)
            .focusRequester(root)
            .focusable()
            .onPreviewKeyEvent { e ->
                // 菜单里有焦点时，Compose 会拿第一下返回键去清除焦点：这里直接关掉菜单
                if (menu && e.key.nativeKeyCode == AndroidKeyEvent.KEYCODE_BACK) {
                    if (e.type == KeyEventType.KeyUp) menu = false
                    return@onPreviewKeyEvent true
                }
                if (menu || confirmExit || e.type != KeyEventType.KeyDown) return@onPreviewKeyEvent false
                when (e.key.nativeKeyCode) {
                    AndroidKeyEvent.KEYCODE_DPAD_CENTER, AndroidKeyEvent.KEYCODE_ENTER, AndroidKeyEvent.KEYCODE_MENU,
                    AndroidKeyEvent.KEYCODE_NUMPAD_ENTER, AndroidKeyEvent.KEYCODE_DPAD_UP, AndroidKeyEvent.KEYCODE_DPAD_DOWN -> {
                        menu = true
                        true
                    }
                    AndroidKeyEvent.KEYCODE_DPAD_LEFT -> {
                        switchDisplay(-1)
                        true
                    }
                    AndroidKeyEvent.KEYCODE_DPAD_RIGHT -> {
                        switchDisplay(1)
                        true
                    }
                    else -> false
                }
            },
    ) {
        AndroidView(factory = { view }, modifier = Modifier.fillMaxSize())

        SessionStatus(session, big = true, onExit = onExit)

        // 被控端的提示（电视只看不操作：不能操作的提示不显示）
        Column(Modifier.align(Alignment.TopCenter).padding(top = 24.dp)) {
            notices.values.mapNotNull { Messages.forViewer(it) }.forEach { n ->
                Text(
                    Messages.notice(ctx, n, os),
                    color = Color.White,
                    fontSize = 16.sp,
                    modifier = Modifier
                        .padding(4.dp)
                        .clip(RoundedCornerShape(12.dp))
                        .background(Color(0xE6261E10))
                        .padding(horizontal = 16.dp, vertical = 8.dp),
                )
            }
        }

        if (info) InfoBar(session, name, Modifier.align(Alignment.TopStart).padding(24.dp))

        AnimatedVisibility(hint && !menu, enter = fadeIn(), exit = fadeOut(), modifier = Modifier.align(Alignment.BottomCenter).padding(bottom = 40.dp)) {
            Text(
                stringResource(R.string.tv_session_hint),
                color = Color.White,
                fontSize = 18.sp,
                modifier = Modifier
                    .clip(RoundedCornerShape(24.dp))
                    .background(Color.Black.copy(alpha = 0.7f))
                    .padding(horizontal = 24.dp, vertical = 12.dp),
            )
        }
        toast?.let {
            Text(
                it,
                color = Color.White,
                fontSize = 20.sp,
                modifier = Modifier
                    .align(Alignment.Center)
                    .clip(RoundedCornerShape(16.dp))
                    .background(Color.Black.copy(alpha = 0.75f))
                    .padding(horizontal = 28.dp, vertical = 16.dp),
            )
        }

        AnimatedVisibility(
            menu,
            enter = slideInHorizontally { it } + fadeIn(),
            exit = slideOutHorizontally { it } + fadeOut(),
            modifier = Modifier.align(Alignment.CenterEnd),
        ) {
            TvMenu(
                session,
                name,
                info,
                onInfo = { info = !info },
                onDisplay = { switchDisplay(1) },
                onDisconnect = {
                    menu = false
                    confirmExit = true
                },
            )
        }
    }

    if (confirmExit) {
        val focus = remember { FocusRequester() }
        AlertDialog(
            onDismissRequest = { confirmExit = false },
            title = { Text(stringResource(R.string.disconnect_title)) },
            text = { Text(stringResource(R.string.disconnect_text, name.ifBlank { Code.format(session.code) })) },
            confirmButton = {
                TextButton(onClick = {
                    confirmExit = false
                    onExit()
                }, modifier = Modifier.focusRequester(focus)) { Text(stringResource(R.string.disconnect)) }
            },
            dismissButton = { TextButton(onClick = { confirmExit = false }) { Text(stringResource(R.string.cancel)) } },
        )
        LaunchedEffect(Unit) {
            delay(100)
            runCatching { focus.requestFocus() }
        }
    }
}

@Composable
private fun InfoBar(session: RemoteSession, name: String, modifier: Modifier) {
    val stats by session.stats.collectAsState()
    val encoder by session.encoder.collectAsState()
    val parts = listOfNotNull(
        name.ifBlank { null },
        encoder?.let { "${it.codec.label} ${it.width}×${it.height}" },
        stats?.let { "${it.fps.roundToInt()} fps" },
        stats?.rttMs?.let { stringResource(R.string.latency_ms, it.roundToInt()) },
        stats?.let { "%.1f Mbps".format(it.kbps / 1000) },
        // 画面走的路：收到画面之后才知道
        stats?.takeIf { it.width > 0 }?.let { stringResource(if (it.direct) R.string.path_direct else R.string.path_relayed) },
    )
    Text(
        parts.joinToString("  ·  "),
        color = Color.White,
        fontSize = 16.sp,
        modifier = modifier
            .clip(RoundedCornerShape(12.dp))
            .background(Color.Black.copy(alpha = 0.6f))
            .padding(horizontal = 16.dp, vertical = 8.dp),
    )
}

@Composable
private fun TvMenu(
    session: RemoteSession,
    name: String,
    info: Boolean,
    onInfo: () -> Unit,
    onDisplay: () -> Unit,
    onDisconnect: () -> Unit,
) {
    val audioOn by session.audioOn.collectAsState()
    val avSyncOn by session.avSyncOn.collectAsState()
    val displays by session.displays.collectAsState()
    val current by session.currentDisplay.collectAsState()
    val q by session.quality.collectAsState()
    val hello by session.hello.collectAsState()
    val app = AirhandApp.instance
    val settings by app.settings.flow.collectAsState()
    val first = remember { FocusRequester() }
    LaunchedEffect(Unit) {
        delay(50)
        runCatching { first.requestFocus() }
    }
    Column(
        Modifier
            .fillMaxHeight()
            .width(380.dp)
            .background(Color(0xF0101420))
            // 项目多，屏幕矮的电视上放不下：焦点移到哪一项就滚到哪一项
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 24.dp, vertical = 28.dp),
    ) {
        Text(name.ifBlank { Code.format(session.code) }, color = Color.White, style = MaterialTheme.typography.headlineSmall)
        Spacer(Modifier.height(20.dp))
        TvMenuItem(
            if (audioOn) Icons.AutoMirrored.Outlined.VolumeUp else Icons.AutoMirrored.Outlined.VolumeOff,
            stringResource(R.string.tv_sound),
            stringResource(if (audioOn) R.string.on else R.string.off),
            Modifier.focusRequester(first),
        ) { session.setAudio(!audioOn) }
        TvMenuItem(Icons.Outlined.Sync, stringResource(R.string.av_sync), stringResource(if (avSyncOn) R.string.on else R.string.off)) {
            session.setAvSync(!avSyncOn)
        }
        if (displays.size > 1) {
            TvMenuItem(Icons.Outlined.Monitor, stringResource(R.string.menu_displays), stringResource(R.string.display_n, current + 1)) { onDisplay() }
        }
        val preset = Preset.match(q) ?: Preset.DEFAULT
        TvMenuItem(Icons.Outlined.HighQuality, stringResource(R.string.menu_quality), presetName(preset)) {
            val next = Preset.entries[(Preset.entries.indexOf(preset) + 1) % Preset.entries.size]
            session.setQuality(next.quality(q.codec, q.priority))
        }
        // 缩放优先（低延迟 / 文字清晰）：对方是电脑时才有区别；选择会记下来
        if (hello?.platform != OsKind.ANDROID) {
            TvMenuItem(Icons.Outlined.Speed, stringResource(R.string.scaling_priority), priorityName(q.priority)) {
                val all = VideoPriority.entries
                session.setPriority(all[(all.indexOf(q.priority) + 1) % all.size])
            }
        }
        // 编码格式：自动 → 双方都支持的各种格式，按一下换一个；选择会记下来
        if (hello != null) {
            val usable = session.usableCodecs().reversed()
            val chosen = Codec.fromWire(settings.codec)?.takeIf { it in usable }
            val choices = listOf<Codec?>(null) + usable
            TvMenuItem(
                Icons.Outlined.VideoSettings,
                stringResource(R.string.codec),
                chosen?.label ?: stringResource(R.string.codec_auto_value, q.codec.label),
            ) { session.setCodec(choices[(choices.indexOf(chosen) + 1) % choices.size]) }
        }
        TvMenuItem(Icons.Outlined.AspectRatio, stringResource(R.string.tv_scale), scaleName(settings.tvScale)) {
            val next = ScaleMode.entries[(settings.tvScale.ordinal + 1) % ScaleMode.entries.size]
            app.updateSettings { it.copy(tvScale = next) }
        }
        TvMenuItem(Icons.Outlined.Info, stringResource(R.string.tv_info), stringResource(if (info) R.string.on else R.string.off)) { onInfo() }
        TvMenuItem(Icons.Outlined.LinkOff, stringResource(R.string.disconnect), null, danger = true) { onDisconnect() }
    }
}

@Composable
fun scaleName(m: ScaleMode): String = stringResource(
    when (m) {
        ScaleMode.FIT -> R.string.tv_scale_fit
        ScaleMode.FILL -> R.string.tv_scale_fill
        ScaleMode.STRETCH -> R.string.tv_scale_stretch
    },
)

@Composable
fun TvMenuItem(
    icon: ImageVector,
    title: String,
    value: String?,
    modifier: Modifier = Modifier,
    danger: Boolean = false,
    onClick: () -> Unit,
) {
    var focused by remember { mutableStateOf(false) }
    Row(
        modifier
            .fillMaxWidth()
            .padding(vertical = 4.dp)
            .clip(RoundedCornerShape(14.dp))
            .background(if (focused) Color.White else Color.Transparent)
            .border(if (focused) 0.dp else 1.dp, Color.White.copy(alpha = 0.08f), RoundedCornerShape(14.dp))
            .onFocusChanged { focused = it.isFocused }
            .onPreviewKeyEvent { e ->
                if (e.type == KeyEventType.KeyUp &&
                    (e.key.nativeKeyCode == AndroidKeyEvent.KEYCODE_DPAD_CENTER || e.key.nativeKeyCode == AndroidKeyEvent.KEYCODE_ENTER || e.key.nativeKeyCode == AndroidKeyEvent.KEYCODE_NUMPAD_ENTER)
                ) {
                    onClick()
                    true
                } else {
                    false
                }
            }
            .focusable()
            .padding(horizontal = 18.dp, vertical = 14.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        val fg = when {
            focused && danger -> Brand.bad
            focused -> Color(0xFF0B0D14)
            danger -> Brand.bad
            else -> Color.White
        }
        Icon(icon, null, tint = fg, modifier = Modifier.size(26.dp))
        Spacer(Modifier.width(16.dp))
        Text(title, color = fg, fontSize = 20.sp, modifier = Modifier.weight(1f))
        value?.let { Text(it, color = fg.copy(alpha = 0.7f), fontSize = 18.sp) }
    }
}

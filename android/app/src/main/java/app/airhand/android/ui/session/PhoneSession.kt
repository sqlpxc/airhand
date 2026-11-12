package app.airhand.android.ui.session

import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectDragGestures
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.isImeVisible
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material.icons.outlined.Close
import androidx.compose.material.icons.outlined.ContentPaste
import androidx.compose.material.icons.outlined.Crop
import androidx.compose.material.icons.automirrored.outlined.HelpOutline
import androidx.compose.material.icons.outlined.HighQuality
import androidx.compose.material.icons.outlined.Sync
import androidx.compose.material.icons.outlined.Keyboard
import androidx.compose.material.icons.outlined.KeyboardCommandKey
import androidx.compose.material.icons.outlined.KeyboardHide
import androidx.compose.material.icons.outlined.LinkOff
import androidx.compose.material.icons.outlined.Monitor
import androidx.compose.material.icons.outlined.Mouse
import androidx.compose.material.icons.outlined.Lock
import androidx.compose.material.icons.outlined.TouchApp
import androidx.compose.material.icons.automirrored.outlined.VolumeOff
import androidx.compose.material.icons.automirrored.outlined.VolumeUp
import androidx.compose.material.icons.outlined.WarningAmber
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.MutableState
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import app.airhand.android.AirhandApp
import app.airhand.android.R
import app.airhand.android.controller.RemoteSession
import app.airhand.android.controller.VideoDecoder
import app.airhand.android.core.Code
import app.airhand.android.core.Codec
import app.airhand.android.core.OsKind
import app.airhand.android.core.Preset
import app.airhand.android.core.SystemAction
import app.airhand.android.core.VideoPriority
import app.airhand.android.data.TouchMode
import app.airhand.android.ui.ConfirmDialog
import app.airhand.android.ui.Messages
import app.airhand.android.ui.OsAvatar
import app.airhand.android.ui.phone.presetDesc
import app.airhand.android.ui.phone.presetName
import app.airhand.android.ui.phone.priorityDesc
import app.airhand.android.ui.phone.priorityName
import app.airhand.android.ui.theme.Brand
import kotlin.math.roundToInt

/** 手机上看到的操作方式：触控板 / 触屏（控制手机时"触屏"就是直通） */
private enum class UiMode { TRACKPAD, TOUCH }

/** 实际的手势方式："触屏"在对方是手机时是直通，对方能接收多点触摸时每个手指都传过去 */
private fun gestureMode(m: UiMode, os: OsKind, touch: Boolean) = when {
    m == UiMode.TRACKPAD -> Gestures.Mode.TRACKPAD
    touch -> Gestures.Mode.MULTITOUCH
    os == OsKind.ANDROID -> Gestures.Mode.PASSTHROUGH
    else -> Gestures.Mode.TOUCH
}

private fun initialMode(setting: TouchMode, os: OsKind) = when (setting) {
    TouchMode.TRACKPAD -> UiMode.TRACKPAD
    TouchMode.TOUCH -> UiMode.TOUCH
    TouchMode.AUTO -> if (os == OsKind.ANDROID) UiMode.TOUCH else UiMode.TRACKPAD
}

private enum class Page { MAIN, SHORTCUTS, DISPLAYS, QUALITY, CLIPBOARD, HELP }

/**
 * 手机上的远程画面。
 *
 * 画面占满屏幕，只有一个可以拖动的小圆钮（点开是菜单），尽量不挡画面。
 * 打开键盘时上面多一排特殊键（Esc、Tab、Ctrl、方向键……），画面缩到键盘上面，始终能看到全貌。
 * 控制手机时底部有返回 / 主页 / 最近任务三个键。第一次使用某种操作方式时先说明手势。
 */
@OptIn(ExperimentalLayoutApi::class, ExperimentalMaterial3Api::class)
@Composable
fun PhoneSession(
    session: RemoteSession,
    view: RemoteView,
    kb: KeyboardController,
    keyboardOpen: MutableState<Boolean>,
    onExit: () -> Unit,
) {
    val app = AirhandApp.instance
    val ctx = LocalContext.current
    val settings by app.settings.flow.collectAsState()
    val video by session.videoSize.collectAsState()
    val cursor by session.cursor.collectAsState()
    val hello by session.hello.collectAsState()
    val hostOs by session.hostOs.collectAsState()
    val name by session.hostName.collectAsState()
    val notices by session.notices.collectAsState()
    val audioError by session.audioError.collectAsState()
    val phase by session.phase.collectAsState()
    // 连上之后以对方报告的平台为准
    val os = hello?.platform?.takeIf { it != OsKind.UNKNOWN } ?: OsKind.of(hostOs)
    val touch = hello?.touch == true
    val actions = hello?.actions ?: emptyList()

    var mode by remember(os) { mutableStateOf(initialMode(settings.touchMode, os)) }
    var menu by remember { mutableStateOf(false) }
    var page by remember { mutableStateOf(Page.MAIN) }
    var confirmExit by remember { mutableStateOf(false) }
    var dismissed by remember { mutableStateOf(setOf<String>()) }
    val gm = gestureMode(mode, os, touch)
    val tipKey = gm.name.lowercase()
    val showTips = phase == RemoteSession.Phase.Live && video != null && tipKey !in settings.tipsSeen

    val displays by session.displays.collectAsState()
    val currentDisplay by session.currentDisplay.collectAsState()
    val mouseLock by session.mouseLock.collectAsState()
    LaunchedEffect(video) { video?.let { view.setVideoSize(it.first, it.second) } }
    LaunchedEffect(cursor) { view.setCursor(cursor) }
    LaunchedEffect(mouseLock) { view.mouseLock = mouseLock }
    // 相对移动按对方屏幕的像素换算
    LaunchedEffect(displays, currentDisplay) {
        displays.firstOrNull { it.index == currentDisplay }?.let { view.setRemoteSize(it.width, it.height) }
    }
    LaunchedEffect(gm) {
        view.gestures.cancel()
        view.gestures.mode = gm
        view.showCursor = gm == Gestures.Mode.TRACKPAD || gm == Gestures.Mode.TOUCH
    }

    // 键盘：按钮 / 三指点击打开；用系统的返回收起时同步状态
    val imeVisible = WindowInsets.isImeVisible
    var imeWasVisible by remember { mutableStateOf(false) }
    LaunchedEffect(keyboardOpen.value) { if (keyboardOpen.value) view.showKeyboard() else view.hideKeyboard() }
    LaunchedEffect(imeVisible) {
        if (imeWasVisible && !imeVisible) keyboardOpen.value = false
        imeWasVisible = imeVisible
    }

    BackHandler(enabled = !menu) { confirmExit = true }

    val imeBottom = WindowInsets.ime.asPaddingValues().calculateBottomPadding()
    val keysBar = if (imeVisible) 44.dp else 0.dp
    val navActions = listOf(SystemAction.BACK, SystemAction.HOME, SystemAction.RECENTS).filter { it in actions }
    val navBar = if (navActions.isNotEmpty() && settings.navKeys && !imeVisible) 48.dp else 0.dp

    Box(Modifier.fillMaxSize().background(Color.Black)) {
        AndroidView(
            factory = { view },
            modifier = Modifier
                .fillMaxSize()
                .padding(bottom = imeBottom + keysBar + navBar),
        )

        val covered = SessionStatus(session, big = false, onExit = onExit)

        // 被控端的提示（采集失败、不能操作……）和本机的问题
        val messages = notices.entries.filter { it.key !in dismissed }.map { it.key to Messages.notice(ctx, it.value, os) } +
            listOfNotNull(audioError?.let { "localAudio" to ctx.getString(R.string.audio_playback_failed, it) }).filter { it.first !in dismissed }
        Column(
            Modifier
                .align(Alignment.TopCenter)
                .windowInsetsPadding(WindowInsets.safeDrawing)
                .padding(top = 8.dp, start = 56.dp, end = 56.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            messages.forEach { (key, text) ->
                NoticePill(text) { dismissed = dismissed + key }
            }
        }

        // 组字中的文字（拼音等），显示在特殊键上方
        if (imeVisible && kb.composing.isNotEmpty()) {
            Surface(
                shape = RoundedCornerShape(10.dp),
                color = Color.Black.copy(alpha = 0.8f),
                modifier = Modifier
                    .align(Alignment.BottomCenter)
                    .padding(bottom = imeBottom + keysBar + 8.dp),
            ) {
                Text(kb.composing, color = Color.White, modifier = Modifier.padding(horizontal = 14.dp, vertical = 8.dp), fontSize = 18.sp)
            }
        }

        if (imeVisible) {
            KeysBar(
                kb,
                os,
                onHide = { keyboardOpen.value = false },
                modifier = Modifier
                    .align(Alignment.BottomCenter)
                    .padding(bottom = imeBottom)
                    .height(keysBar),
            )
        } else if (navBar > 0.dp) {
            AndroidNavBar(
                navActions,
                { session.system(it) },
                Modifier
                    .align(Alignment.BottomCenter)
                    .height(navBar),
            )
        }

        if (!covered) {
            MenuHandle(
                bottomInset = imeBottom + keysBar + navBar,
                onClick = {
                    page = Page.MAIN
                    menu = true
                },
            )
        }

        if (showTips && !covered) {
            GestureTips(gm, onDone = {
                app.updateSettings { it.copy(tipsSeen = it.tipsSeen + tipKey) }
            })
        }
    }

    if (menu) {
        val sheet = rememberModalBottomSheetState(skipPartiallyExpanded = true)
        ModalBottomSheet(
            onDismissRequest = { menu = false },
            sheetState = sheet,
            containerColor = MaterialTheme.colorScheme.surfaceContainer,
        ) {
            BackHandler(enabled = page != Page.MAIN) { page = Page.MAIN }
            Column(
                Modifier
                    .verticalScroll(rememberScrollState())
                    .padding(bottom = 16.dp),
            ) {
                when (page) {
                    Page.MAIN -> MainMenu(
                        session, view, name, os, mode,
                        onMode = { mode = it },
                        onKeyboard = {
                            menu = false
                            keyboardOpen.value = true
                        },
                        onPage = { page = it },
                        onClose = { menu = false },
                        onDisconnect = {
                            menu = false
                            confirmExit = true
                        },
                    )
                    Page.SHORTCUTS -> SubPage(stringResource(R.string.menu_shortcuts), { page = Page.MAIN }) {
                        if (actions.isNotEmpty()) {
                            // 手机：系统操作（对方列出的）
                            SystemActions(actions) { a ->
                                session.system(a)
                                menu = false
                            }
                        } else {
                            Shortcuts(os) { codes ->
                                kb.chord(codes)
                                menu = false
                            }
                        }
                    }
                    Page.DISPLAYS -> SubPage(stringResource(R.string.menu_displays), { page = Page.MAIN }) {
                        Displays(session) { menu = false }
                    }
                    Page.QUALITY -> SubPage(stringResource(R.string.menu_quality), { page = Page.MAIN }) {
                        Quality(session)
                    }
                    Page.CLIPBOARD -> SubPage(stringResource(R.string.menu_clipboard), { page = Page.MAIN }) {
                        ClipboardActions(session) { menu = false }
                    }
                    Page.HELP -> SubPage(stringResource(R.string.menu_help), { page = Page.MAIN }) {
                        GestureHelp(gm)
                    }
                }
            }
        }
    }

    if (confirmExit) {
        ConfirmDialog(
            title = stringResource(R.string.disconnect_title),
            text = stringResource(R.string.disconnect_text, name.ifBlank { Code.format(session.code) }),
            confirm = stringResource(R.string.disconnect),
            destructive = true,
            onDismiss = { confirmExit = false },
            onConfirm = {
                confirmExit = false
                onExit()
            },
        )
    }
}

// ───────────────────────────── 浮动圆钮 ─────────────────────────────

/** 可以拖动的菜单圆钮，松手后贴到最近的左 / 右边缘 */
@Composable
private fun MenuHandle(bottomInset: androidx.compose.ui.unit.Dp, onClick: () -> Unit) {
    val density = LocalDensity.current
    var side by rememberSaveable { mutableFloatStateOf(1f) } // 0 左 1 右
    var fy by rememberSaveable { mutableFloatStateOf(0.35f) } // 纵向位置（比例）
    var drag by remember { mutableStateOf<Pair<Float, Float>?>(null) }
    val size = 48.dp
    BoxWithConstraints(
        Modifier
            .fillMaxSize()
            .windowInsetsPadding(WindowInsets.safeDrawing)
            .padding(bottom = bottomInset),
    ) {
        val wPx = with(density) { maxWidth.toPx() }
        val hPx = with(density) { maxHeight.toPx() }
        val sPx = with(density) { size.toPx() }
        val margin = with(density) { 8.dp.toPx() }
        val restX = if (side < 0.5f) margin else wPx - sPx - margin
        val restY = (fy * (hPx - sPx)).coerceIn(0f, (hPx - sPx).coerceAtLeast(0f))
        val (x, y) = drag ?: (restX to restY)
        Box(
            Modifier
                .offset { IntOffset(x.roundToInt(), y.roundToInt()) }
                .size(size)
                .alpha(if (drag != null) 1f else 0.72f)
                .clip(CircleShape)
                .background(Color.Black.copy(alpha = 0.55f))
                .border(1.dp, Color.White.copy(alpha = 0.25f), CircleShape)
                .pointerInput(wPx, hPx) {
                    detectDragGestures(
                        onDragStart = { drag = restX to restY },
                        onDragEnd = {
                            val (dx, dy) = drag ?: return@detectDragGestures
                            side = if (dx + sPx / 2 < wPx / 2) 0f else 1f
                            fy = if (hPx > sPx) (dy / (hPx - sPx)).coerceIn(0f, 1f) else 0f
                            drag = null
                        },
                        onDragCancel = { drag = null },
                    ) { change, amount ->
                        change.consume()
                        val (cx, cy) = drag ?: (restX to restY)
                        drag = (cx + amount.x).coerceIn(0f, wPx - sPx) to (cy + amount.y).coerceIn(0f, hPx - sPx)
                    }
                }
                .clickable(onClick = onClick),
            contentAlignment = Alignment.Center,
        ) {
            Box(Modifier.size(30.dp).clip(RoundedCornerShape(8.dp)).background(Brand.gradient), contentAlignment = Alignment.Center) {
                androidx.compose.foundation.Image(painterResource(R.drawable.ic_launcher_foreground), stringResource(R.string.menu), Modifier.size(44.dp))
            }
        }
    }
}

// ───────────────────────────── 菜单 ─────────────────────────────

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun MainMenu(
    session: RemoteSession,
    view: RemoteView,
    name: String,
    os: OsKind,
    mode: UiMode,
    onMode: (UiMode) -> Unit,
    onKeyboard: () -> Unit,
    onPage: (Page) -> Unit,
    onClose: () -> Unit,
    onDisconnect: () -> Unit,
) {
    val stats by session.stats.collectAsState()
    val encoder by session.encoder.collectAsState()
    val displays by session.displays.collectAsState()
    val audioOn by session.audioOn.collectAsState()
    val avSyncOn by session.avSyncOn.collectAsState()
    val mouseLock by session.mouseLock.collectAsState()
    val hostOs by session.hostOs.collectAsState()

    Row(Modifier.padding(horizontal = 20.dp, vertical = 4.dp), verticalAlignment = Alignment.CenterVertically) {
        OsAvatar(hostOs, 40.dp)
        Spacer(Modifier.width(12.dp))
        Column(Modifier.weight(1f)) {
            Text(name.ifBlank { Code.format(session.code) }, style = MaterialTheme.typography.titleMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
            val parts = listOfNotNull(
                encoder?.codec?.label,
                stats?.let { "${it.fps.roundToInt()} fps" },
                stats?.rttMs?.let { stringResource(R.string.latency_ms, it.roundToInt()) },
                stats?.let { if (it.kbps >= 1000) "%.1f Mbps".format(it.kbps / 1000) else "${it.kbps.roundToInt()} kbps" },
                // 画面走的路：收到画面之后才知道
                stats?.takeIf { it.width > 0 }?.let { stringResource(if (it.direct) R.string.path_direct else R.string.path_relayed) },
            )
            Text(
                parts.joinToString(" · ").ifEmpty { Code.format(session.code) },
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        IconButton(onClick = onClose) { Icon(Icons.Outlined.Close, stringResource(R.string.close)) }
    }
    Spacer(Modifier.height(8.dp))

    // 操作方式：两个大按钮，一眼看出当前是哪种
    Row(Modifier.padding(horizontal = 16.dp), horizontalArrangement = Arrangement.spacedBy(10.dp)) {
        ModeCard(Icons.Outlined.Mouse, stringResource(R.string.mode_trackpad), stringResource(R.string.mode_trackpad_short), mode == UiMode.TRACKPAD, Modifier.weight(1f)) {
            onMode(UiMode.TRACKPAD)
        }
        ModeCard(
            Icons.Outlined.TouchApp,
            stringResource(R.string.mode_touch),
            stringResource(if (os == OsKind.ANDROID) R.string.mode_touch_phone_short else R.string.mode_touch_short),
            mode == UiMode.TOUCH,
            Modifier.weight(1f),
        ) { onMode(UiMode.TOUCH) }
    }
    Spacer(Modifier.height(12.dp))

    FlowRow(
        Modifier.padding(horizontal = 8.dp),
        maxItemsInEachRow = 4,
    ) {
        val cell = Modifier.weight(1f)
        Action(Icons.Outlined.Keyboard, stringResource(R.string.menu_keyboard), cell, onClick = onKeyboard)
        Action(Icons.Outlined.KeyboardCommandKey, stringResource(R.string.menu_shortcuts), cell) { onPage(Page.SHORTCUTS) }
        Action(if (audioOn) Icons.AutoMirrored.Outlined.VolumeUp else Icons.AutoMirrored.Outlined.VolumeOff, stringResource(if (audioOn) R.string.menu_sound_on else R.string.menu_sound_off), cell, active = audioOn) {
            session.setAudio(!audioOn)
        }
        Action(Icons.Outlined.Sync, stringResource(if (avSyncOn) R.string.menu_av_sync_on else R.string.menu_av_sync_off), cell, active = avSyncOn, enabled = audioOn) {
            session.setAvSync(!avSyncOn)
        }
        Action(Icons.Outlined.HighQuality, stringResource(R.string.menu_quality), cell) { onPage(Page.QUALITY) }
        Action(Icons.Outlined.ContentPaste, stringResource(R.string.menu_clipboard), cell) { onPage(Page.CLIPBOARD) }
        if (displays.size > 1) {
            Action(Icons.Outlined.Monitor, stringResource(R.string.menu_displays), cell) { onPage(Page.DISPLAYS) }
        }
        // 锁定鼠标（控制电脑时）：鼠标一直只发移动了多少，游戏里转视角用
        val lockable = os != OsKind.ANDROID
        if (lockable) {
            Action(Icons.Outlined.Lock, stringResource(if (mouseLock) R.string.menu_mouse_lock_on else R.string.menu_mouse_lock_off), cell, active = mouseLock) {
                session.mouseLock.value = !mouseLock
            }
        }
        Action(Icons.Outlined.Crop, stringResource(R.string.menu_fit), cell, enabled = view.zoomed) {
            view.fit()
            onClose()
        }
        Action(Icons.AutoMirrored.Outlined.HelpOutline, stringResource(R.string.menu_help), cell) { onPage(Page.HELP) }
        Action(Icons.Outlined.LinkOff, stringResource(R.string.disconnect), cell, danger = true, onClick = onDisconnect)
        // 补齐最后一行，保持格子宽度一致
        val items = 9 + (if (displays.size > 1) 1 else 0) + (if (lockable) 1 else 0)
        repeat((4 - items % 4) % 4) { Spacer(cell) }
    }
}

@Composable
private fun ModeCard(icon: ImageVector, title: String, desc: String, selected: Boolean, modifier: Modifier, onClick: () -> Unit) {
    val c = MaterialTheme.colorScheme
    Surface(
        onClick = onClick,
        shape = RoundedCornerShape(16.dp),
        color = if (selected) c.primaryContainer else c.surfaceContainerHigh,
        border = if (selected) androidx.compose.foundation.BorderStroke(2.dp, c.primary) else null,
        modifier = modifier,
    ) {
        Column(Modifier.padding(14.dp)) {
            Icon(icon, null, tint = if (selected) c.primary else c.onSurfaceVariant)
            Spacer(Modifier.height(6.dp))
            Text(title, style = MaterialTheme.typography.titleSmall)
            Text(desc, style = MaterialTheme.typography.bodySmall, color = c.onSurfaceVariant)
        }
    }
}

@Composable
private fun Action(
    icon: ImageVector,
    label: String,
    modifier: Modifier,
    active: Boolean = false,
    enabled: Boolean = true,
    danger: Boolean = false,
    onClick: () -> Unit,
) {
    val c = MaterialTheme.colorScheme
    Column(
        modifier
            .padding(4.dp)
            .clip(RoundedCornerShape(16.dp))
            .clickable(enabled = enabled, onClick = onClick)
            .padding(vertical = 10.dp)
            .alpha(if (enabled) 1f else 0.38f),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Box(
            Modifier
                .size(48.dp)
                .clip(CircleShape)
                .background(
                    when {
                        danger -> c.errorContainer
                        active -> c.primaryContainer
                        else -> c.surfaceContainerHighest
                    },
                ),
            contentAlignment = Alignment.Center,
        ) {
            Icon(icon, null, tint = if (danger) c.error else if (active) c.primary else c.onSurface)
        }
        Spacer(Modifier.height(6.dp))
        Text(label, style = MaterialTheme.typography.labelMedium, textAlign = TextAlign.Center, maxLines = 2)
    }
}

@Composable
private fun SubPage(title: String, onBack: () -> Unit, content: @Composable () -> Unit) {
    Row(Modifier.padding(horizontal = 8.dp), verticalAlignment = Alignment.CenterVertically) {
        IconButton(onClick = onBack) { Icon(Icons.AutoMirrored.Outlined.ArrowBack, stringResource(R.string.back)) }
        Text(title, style = MaterialTheme.typography.titleMedium)
    }
    Spacer(Modifier.height(4.dp))
    content()
}

private data class Combo(val label: Int, val codes: List<String>)

private fun combos(os: OsKind): List<Combo> = when (os) {
    OsKind.MAC -> listOf(
        Combo(R.string.combo_cmd_tab, listOf("MetaLeft", "Tab")),
        Combo(R.string.combo_spotlight, listOf("MetaLeft", "Space")),
        Combo(R.string.combo_mission_control, listOf("ControlLeft", "ArrowUp")),
        Combo(R.string.combo_cmd_w, listOf("MetaLeft", "KeyW")),
        Combo(R.string.combo_cmd_q, listOf("MetaLeft", "KeyQ")),
        Combo(R.string.combo_force_quit, listOf("MetaLeft", "AltLeft", "Escape")),
        Combo(R.string.combo_lock_mac, listOf("ControlLeft", "MetaLeft", "KeyQ")),
        Combo(R.string.combo_screenshot_mac, listOf("MetaLeft", "ShiftLeft", "Digit5")),
        Combo(R.string.combo_copy_mac, listOf("MetaLeft", "KeyC")),
        Combo(R.string.combo_paste_mac, listOf("MetaLeft", "KeyV")),
        Combo(R.string.combo_undo_mac, listOf("MetaLeft", "KeyZ")),
    )
    else -> listOf(
        Combo(R.string.combo_ctrl_alt_del, listOf("ControlLeft", "AltLeft", "Delete")),
        Combo(R.string.combo_alt_tab, listOf("AltLeft", "Tab")),
        Combo(R.string.combo_win, listOf("MetaLeft")),
        Combo(R.string.combo_win_d, listOf("MetaLeft", "KeyD")),
        Combo(R.string.combo_win_e, listOf("MetaLeft", "KeyE")),
        Combo(R.string.combo_win_l, listOf("MetaLeft", "KeyL")),
        Combo(R.string.combo_alt_f4, listOf("AltLeft", "F4")),
        Combo(R.string.combo_task_manager, listOf("ControlLeft", "ShiftLeft", "Escape")),
        Combo(R.string.combo_copy, listOf("ControlLeft", "KeyC")),
        Combo(R.string.combo_paste, listOf("ControlLeft", "KeyV")),
        Combo(R.string.combo_undo, listOf("ControlLeft", "KeyZ")),
        Combo(R.string.combo_print_screen, listOf("PrintScreen")),
    )
}

@Composable
private fun Shortcuts(os: OsKind, onSend: (List<String>) -> Unit) {
    combos(os).forEach { c ->
        Text(
            stringResource(c.label),
            style = MaterialTheme.typography.bodyLarge,
            modifier = Modifier
                .fillMaxWidth()
                .clickable { onSend(c.codes) }
                .padding(horizontal = 24.dp, vertical = 14.dp),
        )
    }
}

/** 系统操作的名字 */
@Composable
fun actionLabel(a: SystemAction): String = stringResource(
    when (a) {
        SystemAction.BACK -> R.string.action_back
        SystemAction.HOME -> R.string.action_home
        SystemAction.RECENTS -> R.string.action_recents
        SystemAction.NOTIFICATIONS -> R.string.action_notifications
        SystemAction.QUICK_SETTINGS -> R.string.action_quick_settings
        SystemAction.SCREENSHOT -> R.string.action_screenshot
        SystemAction.LOCK_SCREEN -> R.string.action_lock_screen
        SystemAction.POWER_MENU -> R.string.action_power_menu
        SystemAction.VOLUME_UP -> R.string.action_volume_up
        SystemAction.VOLUME_DOWN -> R.string.action_volume_down
    },
)

@Composable
private fun SystemActions(actions: List<SystemAction>, onSend: (SystemAction) -> Unit) {
    actions.forEach { a ->
        Text(
            actionLabel(a),
            style = MaterialTheme.typography.bodyLarge,
            modifier = Modifier
                .fillMaxWidth()
                .clickable { onSend(a) }
                .padding(horizontal = 24.dp, vertical = 14.dp),
        )
    }
}

@Composable
private fun Displays(session: RemoteSession, onDone: () -> Unit) {
    val displays by session.displays.collectAsState()
    val current by session.currentDisplay.collectAsState()
    displays.forEach { d ->
        Row(
            Modifier
                .fillMaxWidth()
                .clickable {
                    session.selectDisplay(d.index)
                    onDone()
                }
                .padding(horizontal = 16.dp, vertical = 10.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            RadioButton(selected = d.index == current, onClick = null)
            Spacer(Modifier.width(12.dp))
            Column {
                Text(
                    stringResource(R.string.display_n, d.index + 1) + if (d.primary) " · " + stringResource(R.string.display_primary) else "",
                    style = MaterialTheme.typography.bodyLarge,
                )
                Text("${d.width} × ${d.height}", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun Quality(session: RemoteSession) {
    val q by session.quality.collectAsState()
    val hello by session.hello.collectAsState()
    val encoder by session.encoder.collectAsState()
    val current = Preset.match(q)
    Preset.entries.forEach { p ->
        Row(
            Modifier
                .fillMaxWidth()
                .clickable { session.setQuality(p.quality(q.codec, q.priority)) }
                .padding(horizontal = 16.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            RadioButton(selected = p == current, onClick = null)
            Spacer(Modifier.width(12.dp))
            Column {
                Text(presetName(p), style = MaterialTheme.typography.bodyLarge)
                Text(presetDesc(p), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
    }
    // 缩放优先：对方是电脑时才有区别（手机自己缩放、编码）
    if (hello?.platform != OsKind.ANDROID) {
        HorizontalDivider(Modifier.padding(vertical = 8.dp, horizontal = 16.dp))
        Text(stringResource(R.string.scaling_priority), style = MaterialTheme.typography.titleSmall, modifier = Modifier.padding(horizontal = 24.dp, vertical = 4.dp))
        FlowRow(Modifier.padding(horizontal = 20.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            VideoPriority.entries.forEach { p ->
                FilterChip(
                    selected = q.priority == p,
                    onClick = { session.setPriority(p) },
                    label = { Text(priorityName(p)) },
                )
            }
        }
        Text(
            priorityDesc(q.priority),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(horizontal = 24.dp, vertical = 4.dp),
        )
    }
    HorizontalDivider(Modifier.padding(vertical = 8.dp, horizontal = 16.dp))
    Text(stringResource(R.string.codec), style = MaterialTheme.typography.titleSmall, modifier = Modifier.padding(horizontal = 24.dp, vertical = 4.dp))
    val hostCodecs = hello?.codecs ?: listOf(Codec.H264)
    FlowRow(Modifier.padding(horizontal = 20.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        Codec.PRIORITY.reversed().forEach { c ->
            val ok = c in hostCodecs && c in VideoDecoder.decodable
            FilterChip(
                selected = q.codec == c,
                onClick = { session.setQuality(q.copy(codec = c)) },
                enabled = ok,
                label = { Text(c.label) },
            )
        }
    }
    encoder?.let { e ->
        Text(
            stringResource(R.string.encoder_info, e.codec.label, e.width, e.height, e.name, stringResource(if (e.hardware) R.string.hardware else R.string.software)),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(horizontal = 24.dp, vertical = 8.dp),
        )
    }
}

@Composable
private fun ClipboardActions(session: RemoteSession, onDone: () -> Unit) {
    val app = AirhandApp.instance
    val ctx = LocalContext.current
    fun withText(f: (String) -> Unit) {
        val t = app.clipboard.read()
        if (t.isNullOrEmpty()) {
            android.widget.Toast.makeText(ctx, R.string.problem_no_clipboard_text, android.widget.Toast.LENGTH_SHORT).show()
        } else {
            f(t)
            onDone()
        }
    }
    MenuRow(stringResource(R.string.clipboard_send), stringResource(R.string.clipboard_send_desc)) {
        withText {
            session.sendClipboard(it)
            android.widget.Toast.makeText(ctx, R.string.clipboard_sent, android.widget.Toast.LENGTH_SHORT).show()
        }
    }
    MenuRow(stringResource(R.string.clipboard_type), stringResource(R.string.clipboard_type_desc)) {
        withText { session.typeText(it) }
    }
}

@Composable
private fun MenuRow(title: String, desc: String, onClick: () -> Unit) {
    Column(
        Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(horizontal = 24.dp, vertical = 12.dp),
    ) {
        Text(title, style = MaterialTheme.typography.bodyLarge)
        Text(desc, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}

// ───────────────────────────── 手势说明 ─────────────────────────────

private fun gestureList(mode: Gestures.Mode): List<Pair<Int, Int>> = when (mode) {
    Gestures.Mode.TRACKPAD -> listOf(
        R.string.g_slide to R.string.g_move_pointer,
        R.string.g_tap to R.string.g_left_click,
        R.string.g_long_press to R.string.g_right_click,
        R.string.g_tap_then_drag to R.string.g_drag,
        R.string.g_two_tap to R.string.g_right_click,
        R.string.g_two_slide to R.string.g_scroll,
        R.string.g_pinch to R.string.g_zoom,
        R.string.g_three_tap to R.string.g_keyboard,
    )
    Gestures.Mode.TOUCH -> listOf(
        R.string.g_tap to R.string.g_click_there,
        R.string.g_long_press to R.string.g_right_click_there,
        R.string.g_slide to R.string.g_drag,
        R.string.g_two_slide to R.string.g_scroll,
        R.string.g_pinch to R.string.g_zoom_pan,
        R.string.g_two_tap to R.string.g_right_click,
        R.string.g_three_tap to R.string.g_keyboard,
    )
    Gestures.Mode.PASSTHROUGH -> listOf(
        R.string.g_touch to R.string.g_same_as_phone,
        R.string.g_pinch to R.string.g_zoom_pan_local,
        R.string.g_nav_keys to R.string.g_nav_keys_desc,
        R.string.g_three_tap to R.string.g_keyboard,
    )
    Gestures.Mode.MULTITOUCH -> listOf(
        R.string.g_touch to R.string.g_same_as_phone,
        R.string.g_fingers to R.string.g_fingers_desc,
        R.string.g_nav_keys to R.string.g_nav_keys_desc,
        R.string.g_menu_keyboard to R.string.g_keyboard,
    )
}

@Composable
private fun GestureHelp(mode: Gestures.Mode) {
    gestureList(mode).forEach { (g, a) ->
        Row(Modifier.padding(horizontal = 24.dp, vertical = 8.dp)) {
            Text(stringResource(g), style = MaterialTheme.typography.bodyMedium, fontWeight = FontWeight.SemiBold, modifier = Modifier.width(130.dp))
            Text(stringResource(a), style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }
}

/** 第一次用某种操作方式时的手势说明 */
@Composable
private fun GestureTips(mode: Gestures.Mode, onDone: () -> Unit) {
    Box(
        Modifier
            .fillMaxSize()
            .background(Color.Black.copy(alpha = 0.6f))
            .clickable(onClick = onDone),
        contentAlignment = Alignment.Center,
    ) {
        Surface(shape = RoundedCornerShape(24.dp), color = MaterialTheme.colorScheme.surfaceContainerHigh, modifier = Modifier.widthIn(max = 420.dp).padding(24.dp)) {
            Column(Modifier.padding(vertical = 20.dp)) {
                Text(
                    stringResource(
                        when (mode) {
                            Gestures.Mode.TRACKPAD -> R.string.tips_trackpad
                            Gestures.Mode.TOUCH -> R.string.tips_touch
                            Gestures.Mode.PASSTHROUGH, Gestures.Mode.MULTITOUCH -> R.string.tips_passthrough
                        },
                    ),
                    style = MaterialTheme.typography.titleMedium,
                    modifier = Modifier.padding(horizontal = 24.dp),
                )
                Spacer(Modifier.height(8.dp))
                GestureHelp(mode)
                Text(
                    stringResource(R.string.tips_menu),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(horizontal = 24.dp, vertical = 8.dp),
                )
                Button(onClick = onDone, modifier = Modifier.align(Alignment.End).padding(horizontal = 20.dp)) {
                    Text(stringResource(R.string.got_it))
                }
            }
        }
    }
}

// ───────────────────────────── 键盘上方的特殊键 / 手机导航键 ─────────────────────────────

@Composable
private fun KeysBar(kb: KeyboardController, os: OsKind, onHide: () -> Unit, modifier: Modifier) {
    val meta = if (os == OsKind.MAC) "⌘" else if (os == OsKind.ANDROID) "◯" else "Win"
    Row(
        modifier
            .fillMaxWidth()
            .background(Color(0xFF1B1F2B)),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Row(
            Modifier
                .weight(1f)
                .horizontalScroll(rememberScrollState())
                .padding(horizontal = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Key("Esc") { kb.press("Escape") }
            Key("Tab") { kb.press("Tab") }
            ModKey("Ctrl", kb, KeyboardController.Mod.CTRL)
            ModKey(if (os == OsKind.MAC) "⌥" else "Alt", kb, KeyboardController.Mod.ALT)
            ModKey("⇧", kb, KeyboardController.Mod.SHIFT)
            ModKey(meta, kb, KeyboardController.Mod.META)
            Key("←") { kb.press("ArrowLeft") }
            Key("↑") { kb.press("ArrowUp") }
            Key("↓") { kb.press("ArrowDown") }
            Key("→") { kb.press("ArrowRight") }
            Key("Del") { kb.press("Delete") }
            Key("Home") { kb.press("Home") }
            Key("End") { kb.press("End") }
            Key("PgUp") { kb.press("PageUp") }
            Key("PgDn") { kb.press("PageDown") }
            for (i in 1..12) Key("F$i") { kb.press("F$i") }
        }
        IconButton(onClick = onHide) { Icon(Icons.Outlined.KeyboardHide, stringResource(R.string.hide_keyboard), tint = Color.White) }
    }
}

@Composable
private fun Key(label: String, onClick: () -> Unit) {
    Box(
        Modifier
            .padding(horizontal = 2.dp, vertical = 4.dp)
            .widthIn(min = 44.dp)
            .height(36.dp)
            .clip(RoundedCornerShape(8.dp))
            .background(Color(0xFF2C3244))
            .clickable(onClick = onClick)
            .padding(horizontal = 10.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(label, color = Color.White, fontSize = 14.sp)
    }
}

@Composable
private fun ModKey(label: String, kb: KeyboardController, m: KeyboardController.Mod) {
    val st = kb.state(m)
    Box(
        Modifier
            .padding(horizontal = 2.dp, vertical = 4.dp)
            .widthIn(min = 48.dp)
            .height(36.dp)
            .clip(RoundedCornerShape(8.dp))
            .background(
                when (st) {
                    KeyboardController.Sticky.LOCK -> Brand.accent
                    KeyboardController.Sticky.ONCE -> Brand.accent.copy(alpha = 0.45f)
                    KeyboardController.Sticky.OFF -> Color(0xFF2C3244)
                },
            )
            .then(if (st == KeyboardController.Sticky.LOCK) Modifier.border(2.dp, Color.White.copy(alpha = 0.7f), RoundedCornerShape(8.dp)) else Modifier)
            .clickable { kb.toggle(m) }
            .padding(horizontal = 10.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(label, color = Color.White, fontSize = 14.sp, fontWeight = if (st != KeyboardController.Sticky.OFF) FontWeight.Bold else FontWeight.Normal)
    }
}

/** 控制手机时的返回 / 主页 / 最近任务（对方能执行哪些就显示哪些） */
@Composable
private fun AndroidNavBar(actions: List<SystemAction>, onAction: (SystemAction) -> Unit, modifier: Modifier) {
    Row(
        modifier
            .fillMaxWidth()
            .background(Color.Black)
            .windowInsetsPadding(WindowInsets.navigationBars),
        horizontalArrangement = Arrangement.SpaceEvenly,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        for (a in actions) {
            val symbol = when (a) {
                SystemAction.BACK -> "◁"
                SystemAction.HOME -> "○"
                else -> "▢"
            }
            NavKey(symbol, actionLabel(a)) { onAction(a) }
        }
    }
}

@Composable
private fun NavKey(symbol: String, desc: String, onClick: () -> Unit) {
    Box(
        Modifier
            .size(width = 72.dp, height = 44.dp)
            .clip(RoundedCornerShape(22.dp))
            .clickable(onClickLabel = desc, onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Text(symbol, color = Color.White, fontSize = 22.sp)
    }
}

@Composable
private fun NoticePill(text: String, onDismiss: () -> Unit) {
    AnimatedVisibility(true, enter = fadeIn(), exit = fadeOut()) {
        Surface(shape = RoundedCornerShape(14.dp), color = Color(0xE6261E10)) {
            Row(Modifier.padding(start = 12.dp, top = 6.dp, bottom = 6.dp, end = 4.dp), verticalAlignment = Alignment.CenterVertically) {
                Icon(Icons.Outlined.WarningAmber, null, tint = Brand.warn, modifier = Modifier.size(18.dp))
                Spacer(Modifier.width(8.dp))
                Text(text, color = Color.White, style = MaterialTheme.typography.bodySmall, modifier = Modifier.weight(1f, fill = false))
                IconButton(onClick = onDismiss, modifier = Modifier.size(32.dp)) {
                    Icon(Icons.Outlined.Close, stringResource(R.string.close), tint = Color.White, modifier = Modifier.size(16.dp))
                }
            }
        }
    }
}

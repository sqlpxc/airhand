package app.airhand.android.ui.tv

import android.content.Intent
import android.os.Bundle
import android.view.KeyEvent as AndroidKeyEvent
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.focusable
import androidx.compose.foundation.layout.Arrangement
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
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.Backspace
import androidx.compose.material.icons.outlined.DeleteOutline
import androidx.compose.material.icons.outlined.Settings
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
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
import androidx.compose.ui.draw.scale
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.input.key.KeyEventType
import androidx.compose.ui.input.key.key
import androidx.compose.ui.input.key.nativeKeyCode
import androidx.compose.ui.input.key.onPreviewKeyEvent
import androidx.compose.ui.input.key.type
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import app.airhand.android.AirhandApp
import app.airhand.android.R
import app.airhand.android.controller.RemoteSession
import app.airhand.android.core.Code
import app.airhand.android.core.Native
import app.airhand.android.data.SavedDevice
import app.airhand.android.ui.Logo
import app.airhand.android.ui.Messages
import app.airhand.android.ui.OsAvatar
import app.airhand.android.ui.ServerPill
import app.airhand.android.ui.phone.validServer
import app.airhand.android.ui.session.SessionActivity
import app.airhand.android.ui.theme.AirhandTheme
import app.airhand.android.ui.theme.Brand
import app.airhand.android.ui.theme.Mono
import kotlinx.coroutines.delay

/**
 * 电视上的主界面（从电视桌面打开）：只能看别的设备的画面。
 *
 * 左边输入控制码（屏幕上的数字键盘，遥控器的数字键也可以直接输），右边是最近连接过的设备
 * （右边的垃圾桶 / 长按确认键 / 菜单键删除）。
 * 全部可以用方向键 + 确认键操作；右上角是设置（服务器、本机名称）。
 */
class TvActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            AirhandTheme(dark = true) {
                var settings by remember { mutableStateOf(false) }
                // Surface 提供文字的默认颜色（深色背景上的浅色字）
                Surface(color = MaterialTheme.colorScheme.background, modifier = Modifier.fillMaxSize()) {
                    if (settings) TvSettings(onBack = { settings = false }) else TvHome(onSettings = { settings = true })
                }
            }
        }
    }
}

@Composable
private fun TvHome(onSettings: () -> Unit) {
    val app = AirhandApp.instance
    val ctx = LocalContext.current
    val server by app.controller.server.collectAsState()
    val devices by app.devices.flow.collectAsState()
    val session by app.session.collectAsState()
    val phase = session?.phase?.collectAsState()?.value
    var code by remember { mutableStateOf("") }
    var error by remember { mutableStateOf<String?>(null) }
    val firstKey = remember { FocusRequester() }
    var opened by remember { mutableStateOf<RemoteSession?>(null) }
    var deleting by remember { mutableStateOf<SavedDevice?>(null) }
    // 对方要配对码（或者对方的身份变了，要重新配对）：数字键盘输入的是这个控制码的配对码
    var pairingFor by remember { mutableStateOf<String?>(null) }
    // 每台设备卡片的焦点：删掉一台之后把焦点放到旁边那一台上（不然焦点丢了，遥控器按不动）
    val cardFocus = remember { HashMap<String, FocusRequester>() }
    var refocus by remember { mutableStateOf<String?>(null) }

    LaunchedEffect(Unit) { runCatching { firstKey.requestFocus() } }
    LaunchedEffect(session, phase) {
        val s = session ?: return@LaunchedEffect
        when (phase) {
            is RemoteSession.Phase.Live -> if (opened !== s) {
                opened = s
                ctx.startActivity(Intent(ctx, SessionActivity::class.java))
            }
            is RemoteSession.Phase.Failed -> {
                error = Messages.problem(ctx, phase.reason)
                if (phase.reason.needsPairing) {
                    pairingFor = s.code
                    code = ""
                }
                app.endSession(s)
            }
            else -> {}
        }
    }

    val busy = phase is RemoteSession.Phase.Connecting || phase is RemoteSession.Phase.Approving
    val approving = phase is RemoteSession.Phase.Approving
    val waitingCode = session?.code.takeIf { phase is RemoteSession.Phase.WaitingHost }

    fun connect(c: String) {
        // 输入的是配对码：带着它连刚才那台
        pairingFor?.let { target ->
            if (c.length != 6) return
            pairingFor = null
            error = null
            code = ""
            app.startSession(target, viewOnly = true, pairing = c)
            return
        }
        val parsed = Code.parse(c) ?: run {
            error = ctx.getString(R.string.problem_invalid_code)
            return
        }
        error = null
        app.startSession(parsed, viewOnly = true)
    }

    fun type(d: Char) {
        if (busy || code.length >= 6) return
        error = null
        code += d
        if (code.length == 6) connect(code)
    }

    BackHandler(enabled = session != null && (busy || waitingCode != null)) { app.endSession() }
    // 在输配对码：返回键放弃，变回输控制码
    BackHandler(enabled = pairingFor != null && session == null) {
        pairingFor = null
        code = ""
        error = null
    }

    fun delete(d: SavedDevice) {
        val i = devices.indexOfFirst { it.code == d.code }
        val rest = devices.filter { it.code != d.code }
        // 正在连接 / 等它上线：先停下（不然连上时又会被记进来）
        if (session?.code == d.code) app.endSession()
        app.devices.delete(d.code)
        cardFocus.remove(d.code)
        refocus = rest.getOrNull(i.coerceIn(0, (rest.size - 1).coerceAtLeast(0)))?.code ?: ""
    }

    LaunchedEffect(refocus) {
        val target = refocus ?: return@LaunchedEffect
        // 等对话框关上、列表重新排好
        delay(150)
        runCatching { (cardFocus[target] ?: firstKey).requestFocus() }
        refocus = null
    }

    Row(
        Modifier
            .fillMaxSize()
            .background(MaterialTheme.colorScheme.background)
            .onPreviewKeyEvent { e ->
                if (e.type != KeyEventType.KeyDown) return@onPreviewKeyEvent false
                val k = e.key.nativeKeyCode
                when {
                    k in AndroidKeyEvent.KEYCODE_0..AndroidKeyEvent.KEYCODE_9 -> {
                        type('0' + (k - AndroidKeyEvent.KEYCODE_0)); true
                    }
                    k in AndroidKeyEvent.KEYCODE_NUMPAD_0..AndroidKeyEvent.KEYCODE_NUMPAD_9 -> {
                        type('0' + (k - AndroidKeyEvent.KEYCODE_NUMPAD_0)); true
                    }
                    k == AndroidKeyEvent.KEYCODE_DEL -> {
                        code = code.dropLast(1); true
                    }
                    else -> false
                }
            }
            .padding(horizontal = 56.dp, vertical = 28.dp),
    ) {
        // 左：输入控制码
        Column(Modifier.weight(1f).fillMaxHeight()) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Logo(44.dp)
                Spacer(Modifier.width(16.dp))
                Text(stringResource(R.string.app_name), style = MaterialTheme.typography.headlineMedium)
                Spacer(Modifier.width(20.dp))
                ServerPill(server)
            }
            Spacer(Modifier.height(20.dp))
            Text(
                stringResource(if (pairingFor != null) R.string.pairing_title else R.string.tv_enter_code),
                style = MaterialTheme.typography.titleLarge,
            )
            Spacer(Modifier.height(2.dp))
            Text(
                stringResource(if (pairingFor != null) R.string.pairing_hint else R.string.tv_enter_code_hint),
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 1,
            )
            Spacer(Modifier.height(14.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(10.dp), verticalAlignment = Alignment.CenterVertically) {
                for (i in 0 until 6) {
                    if (i == 3) Spacer(Modifier.width(8.dp))
                    Box(
                        Modifier
                            .size(width = 50.dp, height = 56.dp)
                            .clip(RoundedCornerShape(12.dp))
                            .background(MaterialTheme.colorScheme.surfaceContainerHighest)
                            .border(
                                2.dp,
                                if (error != null) MaterialTheme.colorScheme.error else if (i == code.length) MaterialTheme.colorScheme.primary else Color.Transparent,
                                RoundedCornerShape(12.dp),
                            ),
                        contentAlignment = Alignment.Center,
                    ) {
                        Text(code.getOrNull(i)?.toString() ?: "", fontFamily = Mono, fontSize = 30.sp, fontWeight = FontWeight.SemiBold)
                    }
                }
                if (busy) {
                    Spacer(Modifier.width(20.dp))
                    CircularProgressIndicator(Modifier.size(32.dp))
                }
            }
            Text(
                error ?: if (approving) stringResource(R.string.approving_tv) else "",
                color = if (error != null) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 1,
                modifier = Modifier.padding(top = 6.dp).height(22.dp),
            )
            Spacer(Modifier.height(6.dp))
            // 数字键盘
            val rows = listOf("123", "456", "789")
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                rows.forEachIndexed { r, row ->
                    Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                        row.forEachIndexed { c, d ->
                            TvKey(d.toString(), if (r == 0 && c == 0) Modifier.focusRequester(firstKey) else Modifier) { type(d) }
                        }
                    }
                }
                Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                    TvKey(icon = Icons.AutoMirrored.Outlined.Backspace) { code = code.dropLast(1); error = null }
                    TvKey("0") { type('0') }
                    TvKey(stringResource(R.string.connect), accent = true, enabled = code.length == 6 && !busy, wide = true) { connect(code) }
                }
            }
        }

        Spacer(Modifier.width(56.dp))

        // 右：最近连接
        Column(Modifier.weight(1f).fillMaxHeight()) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(stringResource(R.string.tv_recent), style = MaterialTheme.typography.titleLarge, modifier = Modifier.weight(1f))
                TvKey(stringResource(R.string.tab_settings), icon = Icons.Outlined.Settings, wide = true) { onSettings() }
            }
            Spacer(Modifier.height(16.dp))
            if (devices.isEmpty()) {
                Text(stringResource(R.string.tv_recent_empty), color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            LazyColumn(Modifier.weight(1f, fill = false), verticalArrangement = Arrangement.spacedBy(10.dp)) {
                items(devices, key = { it.code }) { d ->
                    val focus = cardFocus.getOrPut(d.code) { FocusRequester() }
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        TvDeviceCard(
                            d,
                            waiting = waitingCode == d.code,
                            connecting = busy && session?.code == d.code,
                            modifier = Modifier.weight(1f).focusRequester(focus),
                            onDelete = { deleting = d },
                        ) {
                            if (waitingCode == d.code) app.endSession() else connect(d.code)
                        }
                        Spacer(Modifier.width(14.dp))
                        TvKey(icon = Icons.Outlined.DeleteOutline) { deleting = d }
                    }
                }
            }
            if (devices.isNotEmpty()) {
                Spacer(Modifier.height(10.dp))
                Text(stringResource(R.string.tv_device_delete_hint), color = MaterialTheme.colorScheme.onSurfaceVariant, style = MaterialTheme.typography.bodyMedium)
            }
        }
    }

    deleting?.let { d ->
        val focus = remember { FocusRequester() }
        AlertDialog(
            onDismissRequest = { deleting = null },
            title = { Text(stringResource(R.string.device_delete_title)) },
            text = { Text(stringResource(R.string.device_delete_text, d.title().ifBlank { Code.format(d.code) })) },
            confirmButton = {
                TextButton(
                    onClick = {
                        deleting = null
                        delete(d)
                    },
                    modifier = Modifier.focusRequester(focus),
                ) { Text(stringResource(R.string.delete), color = MaterialTheme.colorScheme.error) }
            },
            dismissButton = { TextButton(onClick = { deleting = null }) { Text(stringResource(R.string.cancel)) } },
        )
        LaunchedEffect(Unit) {
            delay(100)
            runCatching { focus.requestFocus() }
        }
    }
}

/**
 * 一台保存的设备。确认键：连接；长按确认键 / 菜单键：删除（松开之后才弹出确认，
 * 免得这次的松开落到对话框的按钮上）
 */
@Composable
private fun TvDeviceCard(
    d: SavedDevice,
    waiting: Boolean,
    connecting: Boolean,
    modifier: Modifier = Modifier,
    onDelete: () -> Unit,
    onClick: () -> Unit,
) {
    val ctx = LocalContext.current
    var focused by remember { mutableStateOf(false) }
    val longPress = remember { booleanArrayOf(false) }
    Row(
        modifier
            .fillMaxWidth()
            .scale(if (focused) 1.03f else 1f)
            .clip(RoundedCornerShape(18.dp))
            .background(if (focused) MaterialTheme.colorScheme.surfaceContainerHighest else MaterialTheme.colorScheme.surfaceContainer)
            .border(2.dp, if (focused) Color.White else Color.Transparent, RoundedCornerShape(18.dp))
            .onFocusChanged { focused = it.isFocused }
            .onPreviewKeyEvent { e ->
                val k = e.key.nativeKeyCode
                when {
                    k == AndroidKeyEvent.KEYCODE_MENU -> {
                        if (e.type == KeyEventType.KeyUp) onDelete()
                        true
                    }
                    k == AndroidKeyEvent.KEYCODE_DPAD_CENTER || k == AndroidKeyEvent.KEYCODE_ENTER || k == AndroidKeyEvent.KEYCODE_NUMPAD_ENTER -> {
                        when (e.type) {
                            // 按住不放时系统会重复发按下事件（大约半秒之后）：算长按
                            KeyEventType.KeyDown -> longPress[0] = e.nativeKeyEvent.repeatCount > 0
                            KeyEventType.KeyUp -> {
                                if (longPress[0]) onDelete() else onClick()
                                longPress[0] = false
                            }
                        }
                        true
                    }
                    else -> false
                }
            }
            .focusable()
            .padding(16.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        OsAvatar(d.hostOs, 52.dp)
        Spacer(Modifier.width(16.dp))
        Column(Modifier.weight(1f)) {
            Text(d.title().ifBlank { Code.format(d.code) }, style = MaterialTheme.typography.titleMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
            Text(
                listOf(Code.format(d.code), d.hostOs, Messages.ago(ctx, d.lastUsed)).filter { it.isNotBlank() }.joinToString(" · "),
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                style = MaterialTheme.typography.bodyMedium,
                maxLines = 1,
            )
            if (waiting) Text(stringResource(R.string.device_waiting_tv), color = Brand.warn, style = MaterialTheme.typography.bodyMedium)
        }
        if (connecting) CircularProgressIndicator(Modifier.size(28.dp))
    }
}

/** 遥控器的确认键 / 回车 = 点击 */
fun Modifier.clickOnCenter(onClick: () -> Unit): Modifier = onPreviewKeyEvent { e ->
    val k = e.key.nativeKeyCode
    if (k == AndroidKeyEvent.KEYCODE_DPAD_CENTER || k == AndroidKeyEvent.KEYCODE_ENTER || k == AndroidKeyEvent.KEYCODE_NUMPAD_ENTER) {
        if (e.type == KeyEventType.KeyUp) onClick()
        true
    } else {
        false
    }
}

@Composable
private fun TvKey(
    label: String? = null,
    modifier: Modifier = Modifier,
    icon: ImageVector? = null,
    accent: Boolean = false,
    enabled: Boolean = true,
    wide: Boolean = false,
    height: androidx.compose.ui.unit.Dp = 48.dp,
    onClick: () -> Unit,
) {
    var focused by remember { mutableStateOf(false) }
    val bg = when {
        focused -> Color.White
        accent && enabled -> MaterialTheme.colorScheme.primary
        else -> MaterialTheme.colorScheme.surfaceContainerHigh
    }
    val fg = when {
        focused -> Color(0xFF0B0D14)
        !enabled -> MaterialTheme.colorScheme.onSurface.copy(alpha = 0.4f)
        else -> MaterialTheme.colorScheme.onSurface
    }
    Row(
        modifier
            .height(height)
            .then(if (wide) Modifier else Modifier.width(if (label != null && label.length > 1) 120.dp else 76.dp))
            .scale(if (focused) 1.06f else 1f)
            .clip(RoundedCornerShape(14.dp))
            .background(bg)
            .onFocusChanged { focused = it.isFocused }
            .clickOnCenter { if (enabled) onClick() }
            .focusable()
            .padding(horizontal = 18.dp),
        horizontalArrangement = Arrangement.Center,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        icon?.let { Icon(it, label, tint = fg) }
        if (icon != null && label != null && wide) Spacer(Modifier.width(10.dp))
        if (label != null && (icon == null || wide)) Text(label, color = fg, fontSize = 22.sp, fontWeight = FontWeight.Medium)
    }
}

@Composable
private fun TvSettings(onBack: () -> Unit) {
    val app = AirhandApp.instance
    val s = app.settings.value
    var server by remember { mutableStateOf(s.server) }
    var fp by remember { mutableStateOf(s.fingerprint) }
    var name by remember { mutableStateOf(s.deviceName) }
    val serverOk = validServer(server)
    val fpError = Native.checkFingerprint(fp)
    val first = remember { FocusRequester() }
    LaunchedEffect(Unit) { runCatching { first.requestFocus() } }
    BackHandler(onBack = onBack)

    Column(
        Modifier
            .fillMaxSize()
            .background(MaterialTheme.colorScheme.background)
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 96.dp, vertical = 28.dp),
    ) {
        Text(stringResource(R.string.tab_settings), style = MaterialTheme.typography.headlineMedium)
        Spacer(Modifier.height(24.dp))
        OutlinedTextField(
            value = server,
            onValueChange = { server = it },
            label = { Text(stringResource(R.string.server_address)) },
            isError = !serverOk,
            supportingText = { Text(if (serverOk) stringResource(R.string.server_address_hint) else stringResource(R.string.server_address_invalid)) },
            singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
            modifier = Modifier.fillMaxWidth().focusRequester(first),
        )
        OutlinedTextField(
            value = fp,
            onValueChange = { fp = it },
            label = { Text(stringResource(R.string.server_fingerprint)) },
            isError = fpError != null,
            supportingText = { Text(if (fpError != null) stringResource(R.string.fingerprint_invalid) else stringResource(R.string.fingerprint_hint)) },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        OutlinedTextField(
            value = name,
            onValueChange = { name = it },
            label = { Text(stringResource(R.string.device_name)) },
            placeholder = { Text(app.settings.deviceName()) },
            supportingText = { Text(stringResource(R.string.device_name_hint)) },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        Spacer(Modifier.height(16.dp))
        Row(horizontalArrangement = Arrangement.spacedBy(16.dp)) {
            TvKey(stringResource(R.string.save), accent = true, enabled = serverOk && fpError == null, wide = true) {
                app.updateSettings {
                    // 改了指纹：第一次连接时记下的那个也不要了（清空指纹 = 下次连接时重新记）
                    val reset = fp.trim() != it.fingerprint
                    it.copy(
                        server = server.trim(),
                        fingerprint = fp.trim(),
                        deviceName = name.trim(),
                        trustedServer = if (reset) "" else it.trustedServer,
                        trustedFp = if (reset) "" else it.trustedFp,
                    )
                }
                onBack()
            }
            TvKey(stringResource(R.string.cancel), wide = true) { onBack() }
        }
        Spacer(Modifier.height(24.dp))
        Text(stringResource(R.string.data_note), color = MaterialTheme.colorScheme.onSurfaceVariant)
        Text(
            stringResource(R.string.version_value, app.packageManager.getPackageInfo(app.packageName, 0).versionName ?: "", Native.protocolVersion()),
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

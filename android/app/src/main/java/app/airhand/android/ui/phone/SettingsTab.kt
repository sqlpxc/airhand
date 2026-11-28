package app.airhand.android.ui.phone

import app.airhand.android.wheelpatch.TabletWheelSettings

import android.content.Intent
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material.icons.automirrored.outlined.KeyboardArrowRight
import androidx.compose.material.icons.outlined.BugReport
import androidx.compose.material.icons.outlined.DeleteSweep
import androidx.compose.material.icons.outlined.Dns
import androidx.compose.material.icons.outlined.Gesture
import androidx.compose.material.icons.outlined.HighQuality
import androidx.compose.material.icons.outlined.Info
import androidx.compose.material.icons.outlined.Key
import androidx.compose.material.icons.outlined.Keyboard
import androidx.compose.material.icons.outlined.Mouse
import androidx.compose.material.icons.outlined.Navigation
import androidx.compose.material.icons.outlined.Refresh
import androidx.compose.material.icons.outlined.Smartphone
import androidx.compose.material.icons.outlined.Speed
import androidx.compose.material.icons.outlined.SportsEsports
import androidx.compose.material.icons.outlined.StayCurrentLandscape
import androidx.compose.material.icons.outlined.SwapVert
import androidx.compose.material.icons.outlined.Sync
import androidx.compose.material.icons.outlined.TouchApp
import androidx.compose.material.icons.outlined.VerifiedUser
import androidx.compose.material.icons.automirrored.outlined.VolumeUp
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItem
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import app.airhand.android.AirhandApp
import app.airhand.android.BuildConfig
import app.airhand.android.R
import app.airhand.android.core.Code
import app.airhand.android.core.CrashLog
import app.airhand.android.core.Native
import app.airhand.android.core.Preset
import app.airhand.android.data.Settings
import app.airhand.android.data.TouchMode
import app.airhand.android.data.WheelMode
import app.airhand.android.host.HostService
import app.airhand.android.host.HostState
import app.airhand.android.ui.ChoiceDialog
import app.airhand.android.ui.ConfirmDialog
import app.airhand.android.ui.EditTextDialog
import app.airhand.android.ui.Messages
import app.airhand.android.ui.SectionTitle
import app.airhand.android.ui.session.Gestures
import app.airhand.android.ui.snapPercent
import app.airhand.android.ui.theme.Brand
import app.airhand.android.ui.theme.Mono
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** 一级设置分类（与会话悬浮窗的分类完全对齐） */
private enum class SettingsCategory {
    VIDEO,
    AUDIO,
    MOUSE_TOUCH,
    KEYBOARD,
    ON_SCREEN_KEYS,
    NETWORK_DEVICE,
    ABOUT,
}

/** "设置"页：一级分类导航 + 二级详情页，所有设置保存在应用私有目录里 */
@Composable
fun SettingsTab(modifier: Modifier, snackbar: SnackbarHostState) {
    val app = AirhandApp.instance
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    val s by app.settings.flow.collectAsState()
    val server by app.controller.server.collectAsState()
    val devices by app.devices.flow.collectAsState()
    val host by HostState.flow.collectAsState()
    val paired = app.pairing.flow.collectAsState().value?.paired?.size ?: 0
    val crashes by CrashLog.summary.collectAsState()
    var category by remember { mutableStateOf<SettingsCategory?>(null) }
    var dialog by remember { mutableStateOf<String?>(null) }
    val scroll = rememberScrollState()

    BackHandler(enabled = category != null) { category = null }
    LaunchedEffect(category) { scroll.scrollTo(0) }

    Column(
        modifier
            .verticalScroll(scroll)
            .padding(bottom = 24.dp),
    ) {
        val current = category
        if (current == null) {
            Text(
                stringResource(R.string.tab_settings),
                style = MaterialTheme.typography.titleLarge,
                modifier = Modifier.padding(start = 20.dp, top = 20.dp, bottom = 8.dp),
            )
            val onText = stringResource(R.string.on)
            val offText = stringResource(R.string.off)
            CategoryRow(
                Icons.Outlined.HighQuality,
                stringResource(R.string.cat_video),
                "${presetName(Preset.of(s.preset))} · ${stringResource(R.string.auto_rotate)} ${if (s.autoRotate) onText else offText}",
            ) { category = SettingsCategory.VIDEO }
            CategoryRow(
                Icons.AutoMirrored.Outlined.VolumeUp,
                stringResource(R.string.cat_audio),
                "${stringResource(R.string.play_audio)} ${if (s.audio) onText else offText} · ${stringResource(R.string.av_sync)} ${if (s.avSync) onText else offText}",
            ) { category = SettingsCategory.AUDIO }
            CategoryRow(
                Icons.Outlined.Mouse,
                stringResource(R.string.cat_mouse_touch),
                "${touchModeName(s.touchMode)} · ${stringResource(R.string.percent, s.wheelSpeed)} / ${stringResource(R.string.percent, s.touchScrollSpeed)}",
            ) { category = SettingsCategory.MOUSE_TOUCH }
            CategoryRow(
                Icons.Outlined.Keyboard,
                stringResource(R.string.cat_keyboard),
                "${stringResource(R.string.double_tap_keyboard)} ${if (s.doubleTapKeyboard) onText else offText}",
            ) { category = SettingsCategory.KEYBOARD }
            CategoryRow(
                Icons.Outlined.SportsEsports,
                stringResource(R.string.cat_on_screen_keys),
                "${stringResource(R.string.nav_keys)} ${if (s.navKeys) onText else offText}",
            ) { category = SettingsCategory.ON_SCREEN_KEYS }
            CategoryRow(
                Icons.Outlined.Dns,
                stringResource(R.string.cat_network_device),
                "${s.server} · ${Messages.server(ctx, server)}",
            ) { category = SettingsCategory.NETWORK_DEVICE }
            // 有崩溃日志时写在这里：首页提示"意外退出过"时点"查看"转到这一页
            CategoryRow(
                Icons.Outlined.Info,
                stringResource(R.string.cat_about_data),
                if (crashes.count > 0) {
                    "${stringResource(R.string.crash_log)} · ${pluralStringResource(R.plurals.crash_log_count, crashes.count, crashes.count, Messages.ago(ctx, crashes.latest))}"
                } else {
                    "${stringResource(R.string.version_value, BuildConfig.VERSION_NAME, Native.protocolVersion())} · ${stringResource(R.string.clear_devices_desc, devices.size)}"
                },
            ) { category = SettingsCategory.ABOUT }
        } else {
            val title = stringResource(
                when (current) {
                    SettingsCategory.VIDEO -> R.string.cat_video
                    SettingsCategory.AUDIO -> R.string.cat_audio
                    SettingsCategory.MOUSE_TOUCH -> R.string.cat_mouse_touch
                    SettingsCategory.KEYBOARD -> R.string.cat_keyboard
                    SettingsCategory.ON_SCREEN_KEYS -> R.string.cat_on_screen_keys
                    SettingsCategory.NETWORK_DEVICE -> R.string.cat_network_device
                    SettingsCategory.ABOUT -> R.string.cat_about_data
                },
            )
            Row(
                Modifier.padding(start = 8.dp, top = 12.dp, end = 20.dp, bottom = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                IconButton(onClick = { category = null }) {
                    Icon(Icons.AutoMirrored.Outlined.ArrowBack, stringResource(R.string.back))
                }
                Text(title, style = MaterialTheme.typography.titleLarge)
            }

            when (current) {
                SettingsCategory.VIDEO -> {
                    Item(Icons.Outlined.HighQuality, stringResource(R.string.default_quality), presetName(Preset.of(s.preset))) { dialog = "preset" }
                    Toggle(Icons.Outlined.Speed, stringResource(R.string.show_stats), stringResource(R.string.show_stats_desc), s.showStats) { v ->
                        app.updateSettings { it.copy(showStats = v) }
                    }
                    Toggle(Icons.Outlined.StayCurrentLandscape, stringResource(R.string.auto_rotate), stringResource(R.string.auto_rotate_desc), s.autoRotate) { v ->
                        app.updateSettings { it.copy(autoRotate = v) }
                    }
                }
                SettingsCategory.AUDIO -> {
                    Toggle(Icons.AutoMirrored.Outlined.VolumeUp, stringResource(R.string.play_audio), stringResource(R.string.play_audio_desc), s.audio) { v ->
                        app.updateSettings { it.copy(audio = v) }
                    }
                    Toggle(Icons.Outlined.Sync, stringResource(R.string.av_sync), stringResource(R.string.av_sync_desc), s.avSync) { v ->
                        app.updateSettings { it.copy(avSync = v) }
                    }
                }
                SettingsCategory.MOUSE_TOUCH -> {
                    Item(Icons.Outlined.Gesture, stringResource(R.string.touch_mode), touchModeName(s.touchMode)) { dialog = "mode" }
                    Toggle(Icons.Outlined.Mouse, stringResource(R.string.local_cursor), stringResource(R.string.local_cursor_desc), s.localCursor) { v ->
                        app.updateSettings { it.copy(localCursor = v) }
                    }
                    SectionTitle(stringResource(R.string.scroll_section_mouse))
                    Item(Icons.Outlined.Speed, stringResource(R.string.wheel_speed), stringResource(R.string.percent, s.wheelSpeed)) { dialog = "wheelSpeed" }
                    Item(Icons.Outlined.Mouse, stringResource(R.string.wheel_mode), wheelModeName(s.wheelMode)) { dialog = "wheelMode" }
                    Toggle(Icons.Outlined.Gesture, stringResource(R.string.smooth_wheel), stringResource(R.string.smooth_wheel_desc), s.smoothWheel) { v ->
                        app.updateSettings { it.copy(smoothWheel = v) }
                    }
                    TabletWheelSettings()
                    Toggle(Icons.Outlined.SwapVert, stringResource(R.string.reverse_wheel), stringResource(R.string.reverse_wheel_desc), s.reverseWheel) { v ->
                        app.updateSettings { it.copy(reverseWheel = v) }
                    }
                    SectionTitle(stringResource(R.string.scroll_section_touch))
                    Item(Icons.Outlined.TouchApp, stringResource(R.string.touch_scroll_speed), stringResource(R.string.percent, s.touchScrollSpeed)) { dialog = "touchScrollSpeed" }
                    Toggle(Icons.Outlined.Gesture, stringResource(R.string.touch_scroll_fling), stringResource(R.string.touch_scroll_fling_desc), s.touchScrollFling) { v ->
                        app.updateSettings { it.copy(touchScrollFling = v) }
                    }
                }
                SettingsCategory.KEYBOARD -> {
                    Toggle(Icons.Outlined.Keyboard, stringResource(R.string.double_tap_keyboard), stringResource(R.string.double_tap_keyboard_desc), s.doubleTapKeyboard) { v ->
                        app.updateSettings { it.copy(doubleTapKeyboard = v) }
                    }
                }
                SettingsCategory.ON_SCREEN_KEYS -> {
                    Toggle(Icons.Outlined.Navigation, stringResource(R.string.nav_keys), stringResource(R.string.nav_keys_desc), s.navKeys) { v ->
                        app.updateSettings { it.copy(navKeys = v) }
                    }
                    Text(
                        stringResource(R.string.on_screen_keys_game_note),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.padding(start = 72.dp, end = 20.dp, top = 4.dp),
                    )
                }
                SettingsCategory.NETWORK_DEVICE -> {
                    SectionTitle(stringResource(R.string.settings_server))
                    Item(Icons.Outlined.Dns, stringResource(R.string.server_address), s.server) { dialog = "server" }
                    val learned = s.learnedFp
                    Item(
                        Icons.Outlined.VerifiedUser,
                        stringResource(R.string.server_fingerprint),
                        when {
                            s.fingerprint.isNotBlank() -> s.fingerprint
                            learned != null -> stringResource(R.string.fingerprint_learned, learned.take(16))
                            else -> stringResource(R.string.fingerprint_none)
                        },
                    ) { dialog = "fingerprint" }
                    Text(
                        Messages.server(ctx, server),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.padding(start = 72.dp, end = 20.dp, bottom = 4.dp),
                    )

                    SectionTitle(stringResource(R.string.settings_this_device))
                    Item(Icons.Outlined.Smartphone, stringResource(R.string.device_name), app.settings.deviceName()) { dialog = "name" }
                    val saved = host.code ?: app.hostCodes.load()?.code?.toString()
                    Item(
                        Icons.Outlined.Refresh,
                        stringResource(R.string.share_new_code),
                        if (saved != null) stringResource(R.string.settings_current_code, Code.format(saved)) else stringResource(R.string.settings_no_code),
                        enabled = saved != null,
                    ) { dialog = "newCode" }
                    Item(
                        Icons.Outlined.VerifiedUser,
                        stringResource(R.string.settings_paired),
                        if (paired > 0) pluralStringResource(R.plurals.settings_paired_count, paired, paired) else stringResource(R.string.settings_paired_none),
                        enabled = paired > 0,
                    ) { dialog = "forgetPaired" }
                }
                SettingsCategory.ABOUT -> {
                    SectionTitle(stringResource(R.string.settings_data))
                    Item(
                        Icons.Outlined.DeleteSweep,
                        stringResource(R.string.clear_devices),
                        stringResource(R.string.clear_devices_desc, devices.size),
                        enabled = devices.isNotEmpty(),
                    ) { dialog = "clear" }
                    Text(
                        stringResource(R.string.data_note),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.padding(start = 72.dp, end = 20.dp, top = 4.dp),
                    )

                    SectionTitle(stringResource(R.string.settings_about))
                    Item(Icons.Outlined.Info, stringResource(R.string.version), stringResource(R.string.version_value, BuildConfig.VERSION_NAME, Native.protocolVersion()))
                    Item(
                        Icons.Outlined.BugReport,
                        stringResource(R.string.crash_log),
                        if (crashes.count == 0) {
                            stringResource(R.string.crash_log_none)
                        } else {
                            pluralStringResource(R.plurals.crash_log_count, crashes.count, crashes.count, Messages.ago(ctx, crashes.latest))
                        },
                        enabled = crashes.count > 0,
                    ) { dialog = "crashLog" }
                }
            }
        }
        Spacer(Modifier.height(8.dp))
    }

    when (dialog) {
        "server" -> EditTextDialog(
            title = stringResource(R.string.server_address),
            initial = s.server,
            placeholder = "relay.example.com:4433",
            supporting = stringResource(R.string.server_address_hint),
            keyboardType = KeyboardType.Uri,
            validate = { v -> if (validServer(v)) null else ctx.getString(R.string.server_address_invalid) },
            onDismiss = { dialog = null },
            onSave = { v ->
                dialog = null
                app.updateSettings { it.copy(server = v.trim()) }
            },
        )
        "fingerprint" -> EditTextDialog(
            title = stringResource(R.string.server_fingerprint),
            initial = s.fingerprint,
            placeholder = "3A:5F:…",
            supporting = stringResource(R.string.fingerprint_hint),
            singleLine = false,
            validate = { v -> Native.checkFingerprint(v)?.let { ctx.getString(R.string.fingerprint_invalid) } },
            onDismiss = { dialog = null },
            onSave = { v ->
                dialog = null
                // 第一次连接时记下的指纹也不要了：填了指纹就用填的，清空了就下次连接时重新记
                app.updateSettings { it.copy(fingerprint = v.trim(), trustedServer = "", trustedFp = "") }
            },
        )
        "name" -> EditTextDialog(
            title = stringResource(R.string.device_name),
            initial = s.deviceName,
            placeholder = app.settings.deviceName(),
            supporting = stringResource(R.string.device_name_hint),
            onDismiss = { dialog = null },
            onSave = { v ->
                dialog = null
                app.updateSettings { it.copy(deviceName = v.trim()) }
            },
        )
        "newCode" -> ConfirmDialog(
            title = stringResource(R.string.share_new_code_title),
            text = stringResource(R.string.share_new_code_text),
            confirm = stringResource(R.string.share_new_code),
            onDismiss = { dialog = null },
            onConfirm = {
                dialog = null
                if (host.running) {
                    HostService.newCode()
                } else {
                    // 没在共享：忘掉保存的控制码，下次共享时服务器分配新的（旧的绑定 180 天不用会被清理）；
                    // 配对也全部重来（和共享时更换控制码一样）：新的配对码，配对过的都忘掉
                    app.hostCodes.save(null)
                    app.pairing.save(null)
                    HostState.update { it.copy(code = null) }
                    scope.launch { snackbar.showSnackbar(ctx.getString(R.string.settings_code_reset)) }
                }
            },
        )
        "forgetPaired" -> ConfirmDialog(
            title = stringResource(R.string.share_forget_paired_title),
            text = stringResource(R.string.share_forget_paired_text),
            confirm = stringResource(R.string.forget),
            destructive = true,
            onDismiss = { dialog = null },
            onConfirm = {
                dialog = null
                app.forgetPaired()
            },
        )
        "preset" -> ChoiceDialog(
            title = stringResource(R.string.default_quality),
            options = Preset.entries,
            selected = Preset.of(s.preset),
            label = { p ->
                Column {
                    Text(presetName(p))
                    Text(presetDesc(p), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            },
            onDismiss = { dialog = null },
            onSelect = { p ->
                dialog = null
                app.updateSettings { it.copy(preset = p.id) }
            },
        )
        "mode" -> ChoiceDialog(
            title = stringResource(R.string.touch_mode),
            options = TouchMode.entries,
            selected = s.touchMode,
            label = { m ->
                Column {
                    Text(touchModeName(m))
                    Text(touchModeDesc(m), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            },
            onDismiss = { dialog = null },
            onSelect = { m ->
                dialog = null
                app.updateSettings { it.copy(touchMode = m) }
            },
        )
        "wheelSpeed" -> PercentSliderDialog(
            title = stringResource(R.string.wheel_speed),
            initial = s.wheelSpeed,
            min = Settings.WHEEL_SPEED_MIN,
            max = Settings.WHEEL_SPEED_MAX,
            step = Settings.WHEEL_SPEED_STEP,
            onDismiss = { dialog = null },
            onSave = { p ->
                dialog = null
                app.updateSettings { it.copy(wheelSpeed = p) }
            },
        )
        "wheelMode" -> ChoiceDialog(
            title = stringResource(R.string.wheel_mode),
            options = WheelMode.entries,
            selected = s.wheelMode,
            label = { m ->
                Column {
                    Text(wheelModeName(m))
                    Text(wheelModeDesc(m), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            },
            onDismiss = { dialog = null },
            onSelect = { m ->
                dialog = null
                app.updateSettings { it.copy(wheelMode = m) }
            },
        )
        "touchScrollSpeed" -> PercentSliderDialog(
            title = stringResource(R.string.touch_scroll_speed),
            initial = s.touchScrollSpeed,
            min = Settings.TOUCH_SCROLL_MIN,
            max = Gestures.maxScrollPercent(LocalDensity.current.density, Settings.TOUCH_SCROLL_STEP),
            step = Settings.TOUCH_SCROLL_STEP,
            onDismiss = { dialog = null },
            onSave = { p ->
                dialog = null
                app.updateSettings { it.copy(touchScrollSpeed = p) }
            },
        )
        "crashLog" -> CrashLogDialog(
            onDismiss = { dialog = null },
            onCopy = { text ->
                dialog = null
                app.clipboard.write(text, "Airhand crash log")
                scope.launch { snackbar.showSnackbar(ctx.getString(R.string.copied)) }
            },
            onShare = { text ->
                dialog = null
                val send = Intent(Intent.ACTION_SEND)
                    .setType("text/plain")
                    .putExtra(Intent.EXTRA_SUBJECT, "Airhand crash log")
                    .putExtra(Intent.EXTRA_TEXT, text)
                runCatching { ctx.startActivity(Intent.createChooser(send, null)) }
            },
            onClear = {
                dialog = null
                scope.launch {
                    withContext(Dispatchers.IO) { CrashLog.clear() }
                    snackbar.showSnackbar(ctx.getString(R.string.crash_log_cleared))
                }
            },
        )
        "clear" -> ConfirmDialog(
            title = stringResource(R.string.clear_devices),
            text = stringResource(R.string.clear_devices_confirm),
            confirm = stringResource(R.string.clear),
            destructive = true,
            onDismiss = { dialog = null },
            onConfirm = {
                dialog = null
                app.devices.clear()
            },
        )
    }
}

/**
 * 崩溃日志：说明记了什么、内容预览（复制之前看得到里面有什么），复制 / 分享 / 清除。
 * 复制、分享的是全部（[CrashLog.export]，不超过 GitHub 一条评论的长度）
 */
@Composable
private fun CrashLogDialog(onDismiss: () -> Unit, onCopy: (String) -> Unit, onShare: (String) -> Unit, onClear: () -> Unit) {
    val text by produceState<String?>(null) { value = withContext(Dispatchers.IO) { CrashLog.export() } }
    val ready = !text.isNullOrEmpty()
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.crash_log)) },
        text = {
            Column {
                Text(stringResource(R.string.crash_log_text), style = MaterialTheme.typography.bodyMedium)
                Spacer(Modifier.height(12.dp))
                Surface(shape = RoundedCornerShape(8.dp), color = MaterialTheme.colorScheme.surfaceContainerHighest) {
                    Text(
                        text?.take(CRASH_PREVIEW_CHARS) ?: "…",
                        fontFamily = Mono,
                        fontSize = 11.sp,
                        lineHeight = 14.sp,
                        modifier = Modifier
                            .fillMaxWidth()
                            .heightIn(max = 220.dp)
                            .verticalScroll(rememberScrollState())
                            .padding(8.dp),
                    )
                }
            }
        },
        confirmButton = {
            Row {
                TextButton(onClick = { text?.let(onShare) }, enabled = ready) { Text(stringResource(R.string.share)) }
                TextButton(onClick = { text?.let(onCopy) }, enabled = ready) { Text(stringResource(R.string.copy)) }
            }
        },
        dismissButton = {
            TextButton(onClick = onClear) { Text(stringResource(R.string.clear), color = MaterialTheme.colorScheme.error) }
        },
    )
}

/** 预览只显示开头这么多（长文本放进 Text 里会卡），复制、分享的是全部 */
private const val CRASH_PREVIEW_CHARS = 8_000

/** `host:port`，端口 1–65535（IPv6 写成 `[::1]:4433`） */
fun validServer(v: String): Boolean {
    val s = v.trim()
    val i = s.lastIndexOf(':')
    if (i <= 0 || i == s.length - 1) return false
    val host = s.substring(0, i)
    val port = s.substring(i + 1).toIntOrNull() ?: return false
    if (port !in 1..65535) return false
    if (host.any { it.isWhitespace() }) return false
    return host.startsWith("[") == host.endsWith("]") && (host.startsWith("[") || !host.contains(':'))
}

@Composable
private fun CategoryRow(icon: ImageVector, title: String, summary: String, onClick: () -> Unit) {
    ListItem(
        headlineContent = { Text(title, style = MaterialTheme.typography.titleSmall) },
        supportingContent = { Text(summary, maxLines = 1, overflow = TextOverflow.Ellipsis) },
        leadingContent = { Icon(icon, null, tint = MaterialTheme.colorScheme.primary) },
        trailingContent = { Icon(Icons.AutoMirrored.Outlined.KeyboardArrowRight, null, tint = MaterialTheme.colorScheme.onSurfaceVariant) },
        colors = ListItemDefaults.colors(containerColor = Color.Transparent),
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(horizontal = 4.dp, vertical = 2.dp),
    )
}

@Composable
private fun Item(
    icon: ImageVector,
    title: String,
    value: String,
    enabled: Boolean = true,
    warn: Boolean = false,
    onClick: (() -> Unit)? = null,
) {
    ListItem(
        headlineContent = { Text(title) },
        supportingContent = { Text(value, color = if (warn) Brand.warn else Color.Unspecified, maxLines = 3) },
        leadingContent = { Icon(icon, null) },
        colors = ListItemDefaults.colors(containerColor = Color.Transparent),
        modifier = Modifier
            .fillMaxWidth()
            .then(if (onClick != null && enabled) Modifier.clickable(onClick = onClick) else Modifier)
            .padding(horizontal = 4.dp),
    )
}

@Composable
private fun Toggle(icon: ImageVector, title: String, desc: String, value: Boolean, onChange: (Boolean) -> Unit) {
    ListItem(
        headlineContent = { Text(title) },
        supportingContent = { Text(desc) },
        leadingContent = { Icon(icon, null) },
        trailingContent = { Switch(checked = value, onCheckedChange = onChange) },
        colors = ListItemDefaults.colors(containerColor = Color.Transparent),
        modifier = Modifier
            .fillMaxWidth()
            .clickable { onChange(!value) }
            .padding(horizontal = 4.dp),
    )
}

@Composable
fun presetName(p: Preset): String = stringResource(
    when (p) {
        Preset.SAVER -> R.string.preset_saver
        Preset.BALANCED -> R.string.preset_balanced
        Preset.SMOOTH -> R.string.preset_smooth
        Preset.HD -> R.string.preset_hd
        Preset.NATIVE -> R.string.preset_native
    },
)

@Composable
fun presetDesc(p: Preset): String = stringResource(
    when (p) {
        Preset.SAVER -> R.string.preset_saver_desc
        Preset.BALANCED -> R.string.preset_balanced_desc
        Preset.SMOOTH -> R.string.preset_smooth_desc
        Preset.HD -> R.string.preset_hd_desc
        Preset.NATIVE -> R.string.preset_native_desc
    },
)


@Composable
fun touchModeName(m: TouchMode): String = stringResource(
    when (m) {
        TouchMode.AUTO -> R.string.mode_auto
        TouchMode.TRACKPAD -> R.string.mode_trackpad
        TouchMode.TOUCH -> R.string.mode_touch
    },
)

@Composable
fun touchModeDesc(m: TouchMode): String = stringResource(
    when (m) {
        TouchMode.AUTO -> R.string.mode_auto_desc
        TouchMode.TRACKPAD -> R.string.mode_trackpad_desc
        TouchMode.TOUCH -> R.string.mode_touch_desc
    },
)

@Composable
fun wheelModeName(m: WheelMode): String = stringResource(
    when (m) {
        WheelMode.ADAPTIVE -> R.string.wheel_mode_adaptive
        WheelMode.LINEAR -> R.string.wheel_mode_linear
    },
)

@Composable
fun wheelModeDesc(m: WheelMode): String = stringResource(
    when (m) {
        WheelMode.ADAPTIVE -> R.string.wheel_mode_adaptive_desc
        WheelMode.LINEAR -> R.string.wheel_mode_linear_desc
    },
)

/** 百分比设置（[min]..[max]，按 [step] 取整）：拖动滑块，保存时才写进设置 */
@Composable
private fun PercentSliderDialog(
    title: String,
    initial: Int,
    min: Int,
    max: Int,
    step: Int,
    onDismiss: () -> Unit,
    onSave: (Int) -> Unit,
) {
    var value by remember(initial, min, max) { mutableIntStateOf(initial.coerceIn(min, max)) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            Column {
                Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
                    Text(stringResource(R.string.percent, min), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    Text(stringResource(R.string.percent, value), style = MaterialTheme.typography.titleMedium, color = MaterialTheme.colorScheme.primary)
                    Text(stringResource(R.string.percent, max), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
                Spacer(Modifier.height(4.dp))
                Slider(
                    value = value.toFloat(),
                    onValueChange = { value = snapPercent(it, min, max, step) },
                    valueRange = min.toFloat()..max.toFloat(),
                )
            }
        },
        confirmButton = { TextButton(onClick = { onSave(value) }) { Text(stringResource(R.string.save)) } },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.cancel)) } },
    )
}

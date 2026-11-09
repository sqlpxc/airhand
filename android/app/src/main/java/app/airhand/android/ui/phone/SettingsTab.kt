package app.airhand.android.ui.phone

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.DeleteSweep
import androidx.compose.material.icons.outlined.Dns
import androidx.compose.material.icons.outlined.Gesture
import androidx.compose.material.icons.outlined.HighQuality
import androidx.compose.material.icons.outlined.Info
import androidx.compose.material.icons.outlined.Key
import androidx.compose.material.icons.outlined.Navigation
import androidx.compose.material.icons.outlined.Refresh
import androidx.compose.material.icons.outlined.Smartphone
import androidx.compose.material.icons.outlined.Sync
import androidx.compose.material.icons.outlined.VerifiedUser
import androidx.compose.material.icons.automirrored.outlined.VolumeUp
import androidx.compose.material3.Icon
import androidx.compose.material3.ListItem
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import app.airhand.android.AirhandApp
import app.airhand.android.BuildConfig
import app.airhand.android.R
import app.airhand.android.core.Code
import app.airhand.android.core.Native
import app.airhand.android.core.Preset
import app.airhand.android.data.TouchMode
import app.airhand.android.host.HostService
import app.airhand.android.host.HostState
import app.airhand.android.ui.ChoiceDialog
import app.airhand.android.ui.ConfirmDialog
import app.airhand.android.ui.EditTextDialog
import app.airhand.android.ui.Messages
import app.airhand.android.ui.SectionTitle
import app.airhand.android.ui.theme.Brand
import kotlinx.coroutines.launch

/** "设置"页。所有设置都保存在应用自己的数据目录里 */
@Composable
fun SettingsTab(modifier: Modifier, snackbar: SnackbarHostState) {
    val app = AirhandApp.instance
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    val s by app.settings.flow.collectAsState()
    val server by app.controller.server.collectAsState()
    val devices by app.devices.flow.collectAsState()
    val host by HostState.flow.collectAsState()
    var dialog by remember { mutableStateOf<String?>(null) }

    Column(
        modifier
            .verticalScroll(rememberScrollState())
            .padding(bottom = 24.dp),
    ) {
        Text(
            stringResource(R.string.tab_settings),
            style = MaterialTheme.typography.titleLarge,
            modifier = Modifier.padding(start = 20.dp, top = 20.dp, bottom = 4.dp),
        )

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

        Toggle(
            Icons.Outlined.VerifiedUser,
            stringResource(R.string.require_approval),
            stringResource(R.string.require_approval_desc),
            s.requireApproval,
        ) { v -> app.updateSettings { it.copy(requireApproval = v) } }

        SectionTitle(stringResource(R.string.settings_remote))
        Item(Icons.Outlined.HighQuality, stringResource(R.string.default_quality), presetName(Preset.of(s.preset))) { dialog = "preset" }
        Item(Icons.Outlined.Gesture, stringResource(R.string.touch_mode), touchModeName(s.touchMode)) { dialog = "mode" }
        Toggle(Icons.AutoMirrored.Outlined.VolumeUp, stringResource(R.string.play_audio), stringResource(R.string.play_audio_desc), s.audio) { v ->
            app.updateSettings { it.copy(audio = v) }
        }
        Toggle(Icons.Outlined.Sync, stringResource(R.string.av_sync), stringResource(R.string.av_sync_desc), s.avSync) { v ->
            app.updateSettings { it.copy(avSync = v) }
        }
        Toggle(Icons.Outlined.Navigation, stringResource(R.string.nav_keys), stringResource(R.string.nav_keys_desc), s.navKeys) { v ->
            app.updateSettings { it.copy(navKeys = v) }
        }

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
                    // 没在共享：忘掉保存的控制码，下次共享时服务器分配新的（旧的绑定 180 天不用会被清理）
                    app.hostCodes.save(null)
                    HostState.update { it.copy(code = null) }
                    scope.launch { snackbar.showSnackbar(ctx.getString(R.string.settings_code_reset)) }
                }
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

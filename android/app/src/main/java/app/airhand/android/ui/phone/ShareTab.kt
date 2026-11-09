package app.airhand.android.ui.phone

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionManager
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.Settings
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.BatteryAlert
import androidx.compose.material.icons.outlined.CheckCircle
import androidx.compose.material.icons.outlined.ContentCopy
import androidx.compose.material.icons.outlined.Notifications
import androidx.compose.material.icons.outlined.Refresh
import androidx.compose.material.icons.automirrored.outlined.ScreenShare
import androidx.compose.material.icons.outlined.Share
import androidx.compose.material.icons.outlined.TouchApp
import androidx.compose.material.icons.automirrored.outlined.VolumeUp
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import app.airhand.android.AirhandApp
import app.airhand.android.R
import app.airhand.android.core.Code
import app.airhand.android.core.ServerState
import app.airhand.android.host.HostService
import app.airhand.android.host.HostState
import app.airhand.android.host.InputService
import app.airhand.android.host.Viewer
import app.airhand.android.ui.ConfirmDialog
import app.airhand.android.ui.Messages
import app.airhand.android.ui.SectionTitle
import app.airhand.android.ui.serverColor
import app.airhand.android.ui.theme.Brand
import app.airhand.android.ui.theme.CodeStyle
import kotlinx.coroutines.launch

/**
 * "共享本机"页：让别人看 / 操作这台手机。
 *
 * 没开始时：说明 + 一个大按钮"开始共享"（系统会弹出录屏确认）。
 * 共享中：大字显示控制码（复制 / 分享），谁在看（可以全部断开），更换控制码，停止共享。
 * 下面始终是需要的权限，每一项说明用途、状态和"去开启"。
 */
@Composable
fun ShareTab(modifier: Modifier, snackbar: SnackbarHostState) {
    val app = AirhandApp.instance
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    val st by HostState.flow.collectAsState()
    var confirmStop by remember { mutableStateOf(false) }
    var confirmNewCode by remember { mutableStateOf(false) }
    var confirmKick by remember { mutableStateOf(false) }
    var showA11yGuide by remember { mutableStateOf(false) }
    val perms = rememberPermissions()

    val projection = rememberLauncherForActivityResult(ActivityResultContracts.StartActivityForResult()) { r ->
        val data = r.data
        if (r.resultCode == Activity.RESULT_OK && data != null) {
            HostService.start(ctx, r.resultCode, data)
        } else {
            scope.launch { snackbar.showSnackbar(ctx.getString(R.string.share_cancelled)) }
        }
    }
    fun askProjection() {
        val mpm = ctx.getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        projection.launch(mpm.createScreenCaptureIntent())
    }
    // 先要通知和录音（声音共享）权限，不管给不给都接着请求录屏
    val runtime = rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
        perms.refresh()
        askProjection()
    }
    fun start() {
        HostState.update { it.copy(stoppedReason = null) }
        val needed = buildList {
            if (Build.VERSION.SDK_INT >= 33 && !perms.notifications) add(Manifest.permission.POST_NOTIFICATIONS)
            if (Build.VERSION.SDK_INT >= 29 && !perms.audio) add(Manifest.permission.RECORD_AUDIO)
        }
        if (needed.isNotEmpty()) runtime.launch(needed.toTypedArray()) else askProjection()
    }

    Column(
        modifier
            .verticalScroll(rememberScrollState())
            .padding(bottom = 24.dp),
    ) {
        Text(
            stringResource(R.string.share_title),
            style = MaterialTheme.typography.titleLarge,
            modifier = Modifier.padding(start = 20.dp, top = 20.dp, bottom = 12.dp),
        )

        if (!st.running) {
            Card(
                Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 16.dp),
                shape = RoundedCornerShape(24.dp),
                colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainer),
            ) {
                Column(Modifier.padding(24.dp), horizontalAlignment = Alignment.CenterHorizontally) {
                    Box(
                        Modifier
                            .size(72.dp)
                            .clip(CircleShape)
                            .background(Brand.gradient),
                        contentAlignment = Alignment.Center,
                    ) {
                        Icon(Icons.AutoMirrored.Outlined.ScreenShare, null, tint = androidx.compose.ui.graphics.Color.White, modifier = Modifier.size(36.dp))
                    }
                    Spacer(Modifier.height(16.dp))
                    Text(stringResource(R.string.share_intro_title), style = MaterialTheme.typography.titleMedium, textAlign = TextAlign.Center)
                    Spacer(Modifier.height(8.dp))
                    Text(
                        stringResource(R.string.share_intro_body),
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        textAlign = TextAlign.Center,
                    )
                    st.stoppedReason?.let {
                        Spacer(Modifier.height(12.dp))
                        Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodyMedium, textAlign = TextAlign.Center)
                    }
                    Spacer(Modifier.height(20.dp))
                    Button(onClick = { start() }, modifier = Modifier.fillMaxWidth().height(52.dp)) {
                        Text(stringResource(R.string.share_start), style = MaterialTheme.typography.titleSmall)
                    }
                }
            }
        } else {
            SharingCard(
                code = st.code,
                server = st.server,
                onCopy = {
                    st.code?.let { app.clipboard.write(Code.format(it)) }
                    scope.launch { snackbar.showSnackbar(ctx.getString(R.string.copied)) }
                },
                onShare = {
                    st.code?.let { c ->
                        val text = ctx.getString(R.string.share_code_message, Code.format(c))
                        ctx.startActivity(
                            Intent.createChooser(Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT, text), null),
                        )
                    }
                },
            )

            // 有人请求连接：最要紧，放在最上面
            st.requests.forEach { r -> RequestCard(r) }

            SectionTitle(stringResource(R.string.share_viewers))
            Card(
                Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 16.dp),
                shape = RoundedCornerShape(20.dp),
                colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainer),
            ) {
                if (st.viewers.isEmpty()) {
                    Text(
                        stringResource(R.string.share_no_viewers),
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        style = MaterialTheme.typography.bodyMedium,
                        modifier = Modifier.padding(20.dp),
                    )
                } else {
                    Column(Modifier.padding(vertical = 8.dp)) {
                        st.viewers.forEach { v ->
                            Row(Modifier.padding(horizontal = 20.dp, vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
                                Box(Modifier.size(8.dp).clip(CircleShape).background(Brand.good))
                                Spacer(Modifier.width(12.dp))
                                Column(Modifier.weight(1f)) {
                                    Text(v.name.ifBlank { stringResource(R.string.unknown_device) }, style = MaterialTheme.typography.bodyLarge)
                                    Text(v.ip, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                                }
                            }
                        }
                        HorizontalDivider(Modifier.padding(vertical = 4.dp), color = MaterialTheme.colorScheme.outlineVariant)
                        TextButton(onClick = { confirmKick = true }, modifier = Modifier.padding(horizontal = 8.dp)) {
                            Text(stringResource(R.string.share_kick_all), color = MaterialTheme.colorScheme.error)
                        }
                    }
                }
            }

            Spacer(Modifier.height(16.dp))
            Row(Modifier.padding(horizontal = 16.dp), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                OutlinedButton(onClick = { confirmNewCode = true }, modifier = Modifier.weight(1f).height(48.dp), enabled = st.server == ServerState.Connected) {
                    Icon(Icons.Outlined.Refresh, null, Modifier.size(18.dp))
                    Spacer(Modifier.width(8.dp))
                    Text(stringResource(R.string.share_new_code_button), maxLines = 1)
                }
                Button(
                    onClick = { if (st.viewers.isEmpty()) HostService.stop(ctx) else confirmStop = true },
                    modifier = Modifier.weight(1f).height(48.dp),
                    colors = ButtonDefaults.buttonColors(containerColor = MaterialTheme.colorScheme.error),
                ) {
                    Text(stringResource(R.string.share_stop))
                }
            }
        }

        SectionTitle(stringResource(R.string.permissions))
        Card(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = 16.dp),
            shape = RoundedCornerShape(20.dp),
            colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainer),
        ) {
            Column(Modifier.padding(vertical = 4.dp)) {
                PermissionRow(
                    icon = Icons.Outlined.TouchApp,
                    title = stringResource(R.string.perm_control),
                    desc = stringResource(R.string.perm_control_desc),
                    granted = perms.accessibility,
                    action = stringResource(R.string.turn_on),
                    onAction = { showA11yGuide = true },
                )
                if (Build.VERSION.SDK_INT >= 29) {
                    PermissionRow(
                        icon = Icons.AutoMirrored.Outlined.VolumeUp,
                        title = stringResource(R.string.perm_audio),
                        desc = stringResource(R.string.perm_audio_desc),
                        granted = perms.audio,
                        action = stringResource(R.string.allow),
                        onAction = { runtime.launch(arrayOf(Manifest.permission.RECORD_AUDIO)) },
                        problem = st.audioProblem.takeIf { st.running },
                    )
                }
                if (Build.VERSION.SDK_INT >= 33) {
                    PermissionRow(
                        icon = Icons.Outlined.Notifications,
                        title = stringResource(R.string.perm_notifications),
                        desc = stringResource(R.string.perm_notifications_desc),
                        granted = perms.notifications,
                        action = stringResource(R.string.allow),
                        onAction = { runtime.launch(arrayOf(Manifest.permission.POST_NOTIFICATIONS)) },
                    )
                }
                PermissionRow(
                    icon = Icons.Outlined.BatteryAlert,
                    title = stringResource(R.string.perm_battery),
                    desc = stringResource(R.string.perm_battery_desc),
                    granted = perms.battery,
                    action = stringResource(R.string.allow),
                    onAction = { requestIgnoreBattery(ctx) },
                )
            }
        }
    }

    if (confirmStop) {
        ConfirmDialog(
            title = stringResource(R.string.share_stop_title),
            text = pluralStringResource(R.plurals.share_stop_text, st.viewers.size, st.viewers.size),
            confirm = stringResource(R.string.share_stop),
            destructive = true,
            onDismiss = { confirmStop = false },
            onConfirm = {
                confirmStop = false
                HostService.stop(ctx)
            },
        )
    }
    if (confirmNewCode) {
        ConfirmDialog(
            title = stringResource(R.string.share_new_code_title),
            text = stringResource(R.string.share_new_code_text),
            confirm = stringResource(R.string.share_new_code),
            onDismiss = { confirmNewCode = false },
            onConfirm = {
                confirmNewCode = false
                HostService.newCode()
            },
        )
    }
    if (confirmKick) {
        ConfirmDialog(
            title = stringResource(R.string.share_kick_all),
            text = stringResource(R.string.share_kick_text),
            confirm = stringResource(R.string.disconnect),
            destructive = true,
            onDismiss = { confirmKick = false },
            onConfirm = {
                confirmKick = false
                HostService.kickAll()
            },
        )
    }
    if (showA11yGuide) {
        AccessibilityGuide(onDismiss = { showA11yGuide = false })
    }
}

/** 连接请求：谁、从哪里，允许 / 拒绝 */
@Composable
private fun RequestCard(r: Viewer) {
    val ctx = LocalContext.current
    Card(
        Modifier
            .fillMaxWidth()
            .padding(start = 16.dp, end = 16.dp, top = 12.dp),
        shape = RoundedCornerShape(20.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.secondaryContainer),
    ) {
        Column(Modifier.padding(20.dp)) {
            Text(
                stringResource(R.string.request_title, r.name.ifBlank { stringResource(R.string.unknown_device) }),
                style = MaterialTheme.typography.titleMedium,
            )
            Spacer(Modifier.height(4.dp))
            val what = stringResource(if (InputService.isEnabled(ctx)) R.string.request_wants_control else R.string.request_wants_view)
            Text(
                stringResource(R.string.request_text, what, r.ip, HostService.REQUEST_TIMEOUT_S),
                style = MaterialTheme.typography.bodyMedium,
            )
            // 安全码：对方屏幕上显示的是同一个，不一样就是中间有人
            r.safety?.let {
                Spacer(Modifier.height(4.dp))
                Text(stringResource(R.string.request_safety, it), style = MaterialTheme.typography.bodyMedium)
            }
            Spacer(Modifier.height(16.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                OutlinedButton(onClick = { HostService.approve(r.id, false) }, modifier = Modifier.weight(1f)) {
                    Text(stringResource(R.string.request_decline))
                }
                Button(onClick = { HostService.approve(r.id, true) }, modifier = Modifier.weight(1f)) {
                    Text(stringResource(R.string.request_allow))
                }
            }
        }
    }
}

@Composable
private fun SharingCard(code: String?, server: ServerState, onCopy: () -> Unit, onShare: () -> Unit) {
    val ctx = LocalContext.current
    Card(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = 16.dp),
        shape = RoundedCornerShape(24.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.primaryContainer),
    ) {
        Column(Modifier.fillMaxWidth().padding(24.dp), horizontalAlignment = Alignment.CenterHorizontally) {
            Text(
                stringResource(R.string.share_your_code),
                style = MaterialTheme.typography.labelLarge,
                color = MaterialTheme.colorScheme.onPrimaryContainer.copy(alpha = 0.8f),
            )
            Spacer(Modifier.height(8.dp))
            Text(
                code?.let { Code.format(it) } ?: "––– –––",
                style = CodeStyle,
                color = MaterialTheme.colorScheme.onPrimaryContainer.copy(alpha = if (server == ServerState.Connected) 1f else 0.45f),
            )
            Spacer(Modifier.height(8.dp))
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(Modifier.size(8.dp).clip(CircleShape).background(serverColor(server)))
                Spacer(Modifier.width(8.dp))
                Text(
                    if (server == ServerState.Connected) stringResource(R.string.share_ready) else Messages.server(ctx, server),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onPrimaryContainer.copy(alpha = 0.8f),
                    textAlign = TextAlign.Center,
                )
            }
            Spacer(Modifier.height(16.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                FilledTonalButton(onClick = onCopy, enabled = code != null) {
                    Icon(Icons.Outlined.ContentCopy, null, Modifier.size(18.dp))
                    Spacer(Modifier.width(8.dp))
                    Text(stringResource(R.string.copy))
                }
                FilledTonalButton(onClick = onShare, enabled = code != null) {
                    Icon(Icons.Outlined.Share, null, Modifier.size(18.dp))
                    Spacer(Modifier.width(8.dp))
                    Text(stringResource(R.string.share))
                }
            }
        }
    }
}

@Composable
private fun PermissionRow(
    icon: ImageVector,
    title: String,
    desc: String,
    granted: Boolean,
    action: String,
    onAction: () -> Unit,
    problem: String? = null,
) {
    Row(Modifier.padding(start = 20.dp, end = 12.dp, top = 12.dp, bottom = 12.dp), verticalAlignment = Alignment.CenterVertically) {
        Icon(icon, null, tint = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.width(16.dp))
        Column(Modifier.weight(1f)) {
            Text(title, style = MaterialTheme.typography.bodyLarge)
            Text(desc, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            problem?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = Brand.warn) }
        }
        Spacer(Modifier.width(8.dp))
        if (granted) {
            Icon(Icons.Outlined.CheckCircle, stringResource(R.string.granted), tint = Brand.good, modifier = Modifier.padding(12.dp))
        } else {
            FilledTonalButton(onClick = onAction) { Text(action) }
        }
    }
}

/** 开启无障碍服务的引导（包括 Android 13 起侧载应用要先"允许受限制的设置"） */
@Composable
private fun AccessibilityGuide(onDismiss: () -> Unit) {
    val ctx = LocalContext.current
    AlertDialog(
        onDismissRequest = onDismiss,
        icon = { Icon(Icons.Outlined.TouchApp, null) },
        title = { Text(stringResource(R.string.a11y_guide_title)) },
        text = {
            Column {
                Text(stringResource(R.string.a11y_guide_steps))
                if (Build.VERSION.SDK_INT >= 33) {
                    Spacer(Modifier.height(12.dp))
                    Text(
                        stringResource(R.string.a11y_guide_restricted),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        },
        confirmButton = {
            TextButton(onClick = {
                onDismiss()
                runCatching { ctx.startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)) }
            }) { Text(stringResource(R.string.open_settings)) }
        },
        dismissButton = {
            if (Build.VERSION.SDK_INT >= 33) {
                TextButton(onClick = {
                    onDismiss()
                    runCatching {
                        ctx.startActivity(
                            Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.fromParts("package", ctx.packageName, null))
                                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
                        )
                    }
                }) { Text(stringResource(R.string.app_info)) }
            } else {
                TextButton(onClick = onDismiss) { Text(stringResource(R.string.cancel)) }
            }
        },
    )
}

/** 各项权限的当前状态（回到前台时刷新） */
class PermissionState(private val ctx: Context) {
    var accessibility by mutableStateOf(false)
    var audio by mutableStateOf(false)
    var notifications by mutableStateOf(false)
    var battery by mutableStateOf(false)
    var version by mutableIntStateOf(0)

    fun refresh() {
        accessibility = InputService.isEnabled(ctx)
        audio = Build.VERSION.SDK_INT < 29 || granted(Manifest.permission.RECORD_AUDIO)
        notifications = Build.VERSION.SDK_INT < 33 || granted(Manifest.permission.POST_NOTIFICATIONS)
        battery = (ctx.getSystemService(Context.POWER_SERVICE) as PowerManager).isIgnoringBatteryOptimizations(ctx.packageName)
        version++
    }

    private fun granted(p: String) = ctx.checkSelfPermission(p) == PackageManager.PERMISSION_GRANTED
}

@Composable
private fun rememberPermissions(): PermissionState {
    val ctx = LocalContext.current
    val owner = LocalLifecycleOwner.current
    val state = remember { PermissionState(ctx).apply { refresh() } }
    val a11y by InputService.connected.collectAsState()
    LaunchedEffect(a11y) { state.refresh() }
    DisposableEffect(owner) {
        val obs = LifecycleEventObserver { _, e -> if (e == Lifecycle.Event.ON_RESUME) state.refresh() }
        owner.lifecycle.addObserver(obs)
        onDispose { owner.lifecycle.removeObserver(obs) }
    }
    return state
}

@SuppressLint("BatteryLife")
private fun requestIgnoreBattery(ctx: Context) {
    val i = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:${ctx.packageName}"))
    runCatching { ctx.startActivity(i) }.onFailure {
        runCatching { ctx.startActivity(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS)) }
    }
}

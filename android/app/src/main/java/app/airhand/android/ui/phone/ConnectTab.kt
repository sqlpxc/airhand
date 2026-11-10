package app.airhand.android.ui.phone

import android.content.ClipDescription
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.ContentCopy
import androidx.compose.material.icons.outlined.ContentPaste
import androidx.compose.material.icons.outlined.DeleteOutline
import androidx.compose.material.icons.outlined.DevicesOther
import androidx.compose.material.icons.outlined.Edit
import androidx.compose.material.icons.outlined.MoreVert
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.SnackbarResult
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import app.airhand.android.AirhandApp
import app.airhand.android.R
import app.airhand.android.controller.RemoteSession
import app.airhand.android.core.Code
import app.airhand.android.core.Problem
import app.airhand.android.core.ServerState
import app.airhand.android.data.SavedDevice
import app.airhand.android.ui.CodeInput
import app.airhand.android.ui.ConfirmDialog
import app.airhand.android.ui.EditTextDialog
import app.airhand.android.ui.Logo
import app.airhand.android.ui.Messages
import app.airhand.android.ui.OsAvatar
import app.airhand.android.ui.SectionTitle
import app.airhand.android.ui.ServerPill
import app.airhand.android.ui.session.SessionActivity
import app.airhand.android.ui.theme.Brand
import kotlinx.coroutines.launch

/**
 * "远程"页：输入控制码连接，或者点一下连接过的设备。
 *
 * 连接的过程留在这一页（按钮 / 设备上转圈），连上了才进入远程画面；连不上就在原地说明原因。
 * 连接过的设备不在线时，在设备上显示"等待上线"，对方一上线就自动连上。
 */
@Composable
fun ConnectTab(modifier: Modifier, snackbar: SnackbarHostState, onOpenSettings: () -> Unit) {
    val app = AirhandApp.instance
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    val focus = LocalFocusManager.current
    val server by app.controller.server.collectAsState()
    val devices by app.devices.flow.collectAsState()
    val session by app.session.collectAsState()
    val phase = session?.phase?.collectAsState()?.value

    var code by rememberSaveable { mutableStateOf("") }
    var error by remember { mutableStateOf<String?>(null) }
    var errorCode by remember { mutableStateOf<String?>(null) }
    var serverDetails by remember { mutableStateOf(false) }
    var renaming by remember { mutableStateOf<SavedDevice?>(null) }
    // 端到端握手要用户决定的失败（要配对码 / 对方的身份变了）：哪个控制码、什么问题
    var askUser by remember { mutableStateOf<Pair<String, Problem>?>(null) }
    var deleting by remember { mutableStateOf<SavedDevice?>(null) }
    val clipHasText = rememberClipHasText()

    // 第一次连接失败：在原地显示原因，会话作废
    LaunchedEffect(session, phase) {
        val s = session ?: return@LaunchedEffect
        if (phase is RemoteSession.Phase.Failed) {
            error = Messages.problem(ctx, phase.reason)
            errorCode = s.code
            if (phase.reason.needsPairing) askUser = s.code to phase.reason
            app.endSession(s)
        }
        // 连上了：这台已经在“我的设备”里，输入框清空
        if (phase is RemoteSession.Phase.Live && Code.parse(code) == s.code) code = ""
    }

    fun connect(c: String) {
        val parsed = Code.parse(c)
        if (parsed == null) {
            error = ctx.getString(R.string.problem_invalid_code)
            errorCode = c
            return
        }
        // 已经在连这台了（连点两下、输入法重复提交）
        val cur = app.session.value
        if (cur != null && cur.code == parsed && cur.phase.value !is RemoteSession.Phase.Failed && cur.phase.value !is RemoteSession.Phase.Closed) return
        error = null
        errorCode = null
        app.startSession(parsed, viewOnly = false)
        focus.clearFocus()
    }

    val busyCode = session?.code.takeIf { phase is RemoteSession.Phase.Connecting || phase is RemoteSession.Phase.Approving }
    val approvingCode = session?.code.takeIf { phase is RemoteSession.Phase.Approving }
    val waitingCode = session?.code.takeIf { phase is RemoteSession.Phase.WaitingHost }

    LazyColumn(modifier, contentPadding = PaddingValues(bottom = 24.dp)) {
        item {
            Row(
                Modifier
                    .fillMaxWidth()
                    .padding(start = 20.dp, end = 16.dp, top = 16.dp, bottom = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Logo(32.dp)
                Spacer(Modifier.width(12.dp))
                Text(stringResource(R.string.app_name), style = MaterialTheme.typography.titleLarge, modifier = Modifier.weight(1f))
                ServerPill(server) { serverDetails = true }
            }
        }

        item {
            Card(
                Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 16.dp, vertical = 8.dp),
                shape = RoundedCornerShape(24.dp),
                colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainer),
            ) {
                Column(Modifier.padding(20.dp)) {
                    Text(stringResource(R.string.connect_title), style = MaterialTheme.typography.titleMedium)
                    Spacer(Modifier.height(4.dp))
                    Text(
                        stringResource(R.string.connect_hint),
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(Modifier.height(16.dp))
                    CodeInput(
                        value = code,
                        onValueChange = {
                            code = it
                            if (errorCode != null) {
                                error = null
                                errorCode = null
                            }
                        },
                        onComplete = { connect(it) },
                        enabled = busyCode == null,
                        isError = error != null && errorCode == code,
                        modifier = Modifier.fillMaxWidth(),
                    )
                    AnimatedVisibility(error != null && (errorCode == null || errorCode == code || code.isEmpty())) {
                        Text(
                            error ?: "",
                            color = MaterialTheme.colorScheme.error,
                            style = MaterialTheme.typography.bodyMedium,
                            modifier = Modifier.padding(top = 12.dp),
                        )
                    }
                    Spacer(Modifier.height(16.dp))
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        if (clipHasText && busyCode == null) {
                            TextButton(onClick = {
                                val found = Code.find(app.clipboard.read())
                                if (found != null) {
                                    code = found
                                    connect(found)
                                } else {
                                    scope.launch { snackbar.showSnackbar(ctx.getString(R.string.connect_no_code_in_clipboard)) }
                                }
                            }) {
                                Icon(Icons.Outlined.ContentPaste, null, Modifier.size(18.dp))
                                Spacer(Modifier.width(8.dp))
                                Text(stringResource(R.string.connect_paste))
                            }
                        }
                        Spacer(Modifier.weight(1f))
                        Button(
                            onClick = { connect(code) },
                            enabled = code.length == 6 && busyCode == null,
                        ) {
                            if (busyCode != null && busyCode == Code.parse(code)) {
                                CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp, color = MaterialTheme.colorScheme.onPrimary)
                                Spacer(Modifier.width(10.dp))
                                Text(stringResource(if (approvingCode != null) R.string.approving_short else R.string.connecting))
                            } else {
                                Text(stringResource(R.string.connect))
                            }
                        }
                    }
                }
            }
        }

        // 对方要先同意（手机默认这样）：说明在等什么，可以取消
        val sa = session
        if (sa != null && phase is RemoteSession.Phase.Approving) {
            item {
                ApprovingCard(
                    name = sa.hostName.collectAsState().value.ifBlank { Code.format(sa.code) },
                    os = sa.hostOs.collectAsState().value,
                    onCancel = { app.endSession(sa) },
                )
            }
        }

        // 会话还在（例如按了主页键离开了远程画面）：一键回去
        val s = session
        if (s != null && (phase is RemoteSession.Phase.Live || phase is RemoteSession.Phase.Reconnecting)) {
            item {
                ActiveSessionCard(
                    name = s.hostName.collectAsState().value.ifBlank { Code.format(s.code) },
                    os = s.hostOs.collectAsState().value,
                    onOpen = { ctx.startActivity(Intent(ctx, SessionActivity::class.java)) },
                    onDisconnect = { app.endSession(s) },
                )
            }
        }

        if (server is ServerState.Disconnected) {
            item {
                ServerProblemCard((server as ServerState.Disconnected).let { Messages.problem(ctx, it.reason) }, onOpenSettings)
            }
        }

        item { SectionTitle(stringResource(R.string.my_devices)) }
        if (devices.isEmpty()) {
            item { EmptyDevices() }
        }
        items(devices, key = { it.code }) { d ->
            DeviceRow(
                d,
                connecting = busyCode == d.code,
                waiting = waitingCode == d.code,
                error = if (errorCode == d.code && code != d.code) error else null,
                onClick = {
                    if (waitingCode == d.code || busyCode == d.code) return@DeviceRow
                    connect(d.code)
                },
                onCancel = { app.endSession() },
                onRename = { renaming = d },
                onCopy = {
                    app.clipboard.write(Code.format(d.code))
                    scope.launch { snackbar.showSnackbar(ctx.getString(R.string.copied)) }
                },
                onDelete = { deleting = d },
            )
        }
    }

    // 对方要配对码、或者对方的身份变了（用户确认是重装了之后重新配对）：输入对方托盘菜单里的配对码再连
    askUser?.let { (c, p) ->
        val hint = stringResource(R.string.pairing_hint)
        EditTextDialog(
            title = stringResource(R.string.pairing_title),
            initial = "",
            label = stringResource(R.string.pairing_label),
            supporting = Messages.problem(ctx, p),
            placeholder = "000000",
            keyboardType = KeyboardType.Number,
            validate = { if (it.count(Char::isDigit) == 6) null else hint },
            onDismiss = { askUser = null },
            onSave = {
                askUser = null
                error = null
                errorCode = null
                app.startSession(c, viewOnly = false, pairing = it.filter(Char::isDigit))
            },
        )
    }
    renaming?.let { d ->
        EditTextDialog(
            title = stringResource(R.string.device_rename),
            initial = d.title(),
            placeholder = d.hostName.ifBlank { Code.format(d.code) },
            onDismiss = { renaming = null },
            onSave = {
                app.devices.rename(d.code, it.takeIf { n -> n != d.hostName })
                renaming = null
            },
        )
    }
    deleting?.let { d ->
        ConfirmDialog(
            title = stringResource(R.string.device_delete_title),
            text = stringResource(R.string.device_delete_text, d.title().ifBlank { Code.format(d.code) }),
            confirm = stringResource(R.string.delete),
            destructive = true,
            onDismiss = { deleting = null },
            onConfirm = {
                deleting = null
                val removed = d
                app.devices.delete(d.code)
                scope.launch {
                    val r = snackbar.showSnackbar(ctx.getString(R.string.device_deleted), ctx.getString(R.string.undo), withDismissAction = true)
                    if (r == SnackbarResult.ActionPerformed) app.devices.restore(removed)
                }
            },
        )
    }
    if (serverDetails) {
        androidx.compose.material3.AlertDialog(
            onDismissRequest = { serverDetails = false },
            title = { Text(stringResource(R.string.server)) },
            text = {
                Column {
                    Text(app.settings.value.server, style = MaterialTheme.typography.bodyLarge)
                    Spacer(Modifier.height(8.dp))
                    Text(Messages.server(ctx, server), color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            },
            confirmButton = { TextButton(onClick = { serverDetails = false }) { Text(stringResource(R.string.close)) } },
            dismissButton = {
                TextButton(onClick = {
                    serverDetails = false
                    onOpenSettings()
                }) { Text(stringResource(R.string.open_settings)) }
            },
        )
    }
}

@Composable
private fun ApprovingCard(name: String, os: String, onCancel: () -> Unit) {
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 16.dp, vertical = 4.dp),
        shape = RoundedCornerShape(16.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.secondaryContainer),
    ) {
        Row(Modifier.padding(start = 16.dp, end = 8.dp, top = 12.dp, bottom = 12.dp), verticalAlignment = Alignment.CenterVertically) {
            Box(contentAlignment = Alignment.Center) {
                OsAvatar(os, 40.dp)
                CircularProgressIndicator(Modifier.size(44.dp), strokeWidth = 2.dp)
            }
            Spacer(Modifier.width(12.dp))
            Column(Modifier.weight(1f)) {
                Text(stringResource(R.string.approving_title, name), style = MaterialTheme.typography.titleSmall, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text(stringResource(R.string.approving_text), style = MaterialTheme.typography.bodySmall)
            }
            TextButton(onClick = onCancel) { Text(stringResource(R.string.cancel)) }
        }
    }
}

@Composable
private fun ActiveSessionCard(name: String, os: String, onOpen: () -> Unit, onDisconnect: () -> Unit) {
    Card(
        onClick = onOpen,
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 16.dp, vertical = 4.dp),
        shape = RoundedCornerShape(16.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.primaryContainer),
    ) {
        Row(Modifier.padding(start = 16.dp, end = 8.dp, top = 12.dp, bottom = 12.dp), verticalAlignment = Alignment.CenterVertically) {
            OsAvatar(os, 40.dp)
            Spacer(Modifier.width(12.dp))
            Column(Modifier.weight(1f)) {
                Text(name, style = MaterialTheme.typography.titleSmall, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text(stringResource(R.string.session_active), style = MaterialTheme.typography.bodySmall)
            }
            TextButton(onClick = onDisconnect) { Text(stringResource(R.string.disconnect)) }
        }
    }
}

@Composable
private fun ServerProblemCard(text: String, onOpenSettings: () -> Unit) {
    Card(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = 16.dp, vertical = 4.dp),
        shape = RoundedCornerShape(16.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.errorContainer),
    ) {
        Row(Modifier.padding(start = 16.dp, end = 8.dp, top = 12.dp, bottom = 12.dp), verticalAlignment = Alignment.CenterVertically) {
            Text(
                text,
                color = MaterialTheme.colorScheme.onErrorContainer,
                style = MaterialTheme.typography.bodyMedium,
                modifier = Modifier.weight(1f),
            )
            TextButton(onClick = onOpenSettings) { Text(stringResource(R.string.open_settings)) }
        }
    }
}

@Composable
private fun EmptyDevices() {
    Column(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = 32.dp, vertical = 24.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Icon(
            Icons.Outlined.DevicesOther,
            null,
            tint = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.6f),
            modifier = Modifier.size(48.dp),
        )
        Spacer(Modifier.height(12.dp))
        Text(
            stringResource(R.string.my_devices_empty),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun DeviceRow(
    d: SavedDevice,
    connecting: Boolean,
    waiting: Boolean,
    error: String?,
    onClick: () -> Unit,
    onCancel: () -> Unit,
    onRename: () -> Unit,
    onCopy: () -> Unit,
    onDelete: () -> Unit,
) {
    val ctx = LocalContext.current
    var menu by remember { mutableStateOf(false) }
    Row(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = 8.dp)
            .clip(RoundedCornerShape(16.dp))
            .clickable(onClick = onClick)
            .padding(start = 12.dp, top = 10.dp, bottom = 10.dp, end = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        OsAvatar(d.hostOs)
        Spacer(Modifier.width(14.dp))
        Column(Modifier.weight(1f)) {
            Text(
                d.title().ifBlank { Code.format(d.code) },
                style = MaterialTheme.typography.titleMedium,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            val sub = listOf(Code.format(d.code), d.hostOs, Messages.ago(ctx, d.lastUsed)).filter { it.isNotBlank() }.joinToString(" · ")
            Text(
                sub,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            when {
                waiting -> Text(stringResource(R.string.device_waiting), style = MaterialTheme.typography.bodySmall, color = Brand.warn)
                error != null -> Text(error, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error)
            }
        }
        when {
            connecting -> Box(Modifier.size(48.dp), contentAlignment = Alignment.Center) {
                CircularProgressIndicator(Modifier.size(22.dp), strokeWidth = 2.dp)
            }
            waiting -> TextButton(onClick = onCancel) { Text(stringResource(R.string.cancel)) }
            else -> Box {
                IconButton(onClick = { menu = true }) { Icon(Icons.Outlined.MoreVert, stringResource(R.string.more)) }
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                    DropdownMenuItem(
                        text = { Text(stringResource(R.string.device_rename)) },
                        leadingIcon = { Icon(Icons.Outlined.Edit, null) },
                        onClick = { menu = false; onRename() },
                    )
                    DropdownMenuItem(
                        text = { Text(stringResource(R.string.copy_code)) },
                        leadingIcon = { Icon(Icons.Outlined.ContentCopy, null) },
                        onClick = { menu = false; onCopy() },
                    )
                    DropdownMenuItem(
                        text = { Text(stringResource(R.string.delete)) },
                        leadingIcon = { Icon(Icons.Outlined.DeleteOutline, null) },
                        onClick = { menu = false; onDelete() },
                    )
                }
            }
        }
    }
}

/** 剪贴板里有没有文本（只看描述，不读内容：Android 12+ 读剪贴板会弹出提示） */
@Composable
private fun rememberClipHasText(): Boolean {
    val ctx = LocalContext.current
    val owner = LocalLifecycleOwner.current
    var has by remember { mutableStateOf(false) }
    DisposableEffect(owner) {
        val cm = ctx.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        fun check() {
            has = runCatching {
                cm.hasPrimaryClip() && cm.primaryClipDescription?.let {
                    it.hasMimeType(ClipDescription.MIMETYPE_TEXT_PLAIN) || it.hasMimeType(ClipDescription.MIMETYPE_TEXT_HTML)
                } == true
            }.getOrDefault(false)
        }
        val listener = ClipboardManager.OnPrimaryClipChangedListener { check() }
        val obs = LifecycleEventObserver { _, e -> if (e == Lifecycle.Event.ON_RESUME) check() }
        owner.lifecycle.addObserver(obs)
        cm.addPrimaryClipChangedListener(listener)
        check()
        onDispose {
            owner.lifecycle.removeObserver(obs)
            cm.removePrimaryClipChangedListener(listener)
        }
    }
    return has
}

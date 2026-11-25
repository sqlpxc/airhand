package app.airhand.android.ui

import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
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
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Computer
import androidx.compose.material.icons.outlined.DesktopWindows
import androidx.compose.material.icons.outlined.LaptopMac
import androidx.compose.material.icons.outlined.PhoneAndroid
import androidx.compose.material.icons.outlined.PhoneIphone
import androidx.compose.material.icons.outlined.Terminal
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import app.airhand.android.R
import app.airhand.android.core.OsKind
import app.airhand.android.core.ServerState
import app.airhand.android.ui.theme.Brand
import app.airhand.android.ui.theme.Mono

/** 应用标志：渐变圆角方块 + 光标 */
@Composable
fun Logo(size: Dp = 36.dp) {
    Box(
        Modifier
            .size(size)
            .clip(RoundedCornerShape(size * 0.24f))
            .background(Brand.gradient),
        contentAlignment = Alignment.Center,
    ) {
        Image(painterResource(R.drawable.ic_launcher_foreground), null, Modifier.size(size * 1.5f))
    }
}

fun osIcon(os: String?): ImageVector = when (OsKind.of(os)) {
    OsKind.WINDOWS -> Icons.Outlined.DesktopWindows
    OsKind.MAC -> Icons.Outlined.LaptopMac
    OsKind.LINUX -> Icons.Outlined.Terminal
    OsKind.ANDROID -> Icons.Outlined.PhoneAndroid
    OsKind.IOS -> Icons.Outlined.PhoneIphone
    OsKind.UNKNOWN -> Icons.Outlined.Computer
}

/** 设备图标（按系统），放在圆形的底色里 */
@Composable
fun OsAvatar(os: String?, size: Dp = 44.dp, tint: Color = MaterialTheme.colorScheme.primary) {
    Box(
        Modifier
            .size(size)
            .clip(CircleShape)
            .background(tint.copy(alpha = 0.14f)),
        contentAlignment = Alignment.Center,
    ) {
        Icon(osIcon(os), null, tint = tint, modifier = Modifier.size(size * 0.52f))
    }
}

fun serverColor(s: ServerState): Color = when (s) {
    ServerState.Connected -> Brand.good
    ServerState.Connecting -> Brand.warn
    is ServerState.Disconnected -> Brand.bad
}

/** 与服务器的连接状态：小圆点 + 文字 */
@Composable
fun ServerPill(state: ServerState, onClick: (() -> Unit)? = null) {
    val text = when (state) {
        ServerState.Connected -> stringResource(R.string.server_online)
        ServerState.Connecting -> stringResource(R.string.server_connecting)
        is ServerState.Disconnected -> stringResource(R.string.server_offline)
    }
    Surface(
        shape = CircleShape,
        color = MaterialTheme.colorScheme.surfaceContainerHigh,
        modifier = if (onClick != null) Modifier.clip(CircleShape).clickable(onClick = onClick) else Modifier,
    ) {
        Row(Modifier.padding(horizontal = 12.dp, vertical = 6.dp), verticalAlignment = Alignment.CenterVertically) {
            Box(Modifier.size(8.dp).clip(CircleShape).background(serverColor(state)))
            Spacer(Modifier.width(8.dp))
            Text(text, style = MaterialTheme.typography.labelMedium)
        }
    }
}

@Composable
fun SectionTitle(text: String, modifier: Modifier = Modifier) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = modifier.padding(start = 20.dp, end = 20.dp, top = 24.dp, bottom = 8.dp),
    )
}

/**
 * 6 位控制码输入框：六个格子，只能输入数字，可以粘贴"123 456"这样的文本。
 * 输满 6 位时调用 [onComplete]。
 */
@Composable
fun CodeInput(
    value: String,
    onValueChange: (String) -> Unit,
    onComplete: (String) -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    isError: Boolean = false,
    focusRequester: FocusRequester = remember { FocusRequester() },
) {
    var tfv by remember { mutableStateOf(TextFieldValue(value, TextRange(value.length))) }
    if (tfv.text != value) tfv = TextFieldValue(value, TextRange(value.length))
    BasicTextField(
        value = tfv,
        onValueChange = { v ->
            val digits = v.text.filter { it.isDigit() }.take(6)
            // 和输入框自己的上一个值比（`value` 要等重组才更新）：同一个值只通知一次
            val old = tfv.text
            tfv = TextFieldValue(digits, TextRange(digits.length))
            if (digits != old) {
                onValueChange(digits)
                if (digits.length == 6) onComplete(digits)
            }
        },
        enabled = enabled,
        singleLine = true,
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.NumberPassword, imeAction = ImeAction.Go),
        keyboardActions = KeyboardActions(onGo = { if (value.length == 6) onComplete(value) }),
        cursorBrush = SolidColor(Color.Transparent),
        textStyle = MaterialTheme.typography.bodyLarge.copy(color = Color.Transparent),
        modifier = modifier.focusRequester(focusRequester),
        decorationBox = { inner ->
            Box {
                Box(Modifier.size(1.dp)) { inner() }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
                    for (i in 0 until 6) {
                        if (i == 3) Spacer(Modifier.width(6.dp))
                        val c = value.getOrNull(i)
                        val active = i == value.length && enabled
                        val border = when {
                            isError -> MaterialTheme.colorScheme.error
                            active -> MaterialTheme.colorScheme.primary
                            else -> MaterialTheme.colorScheme.outlineVariant
                        }
                        Box(
                            Modifier
                                .weight(1f)
                                .height(56.dp)
                                .clip(RoundedCornerShape(12.dp))
                                .background(MaterialTheme.colorScheme.surfaceContainerHighest.copy(alpha = 0.6f))
                                .border(if (active || isError) 2.dp else 1.dp, border, RoundedCornerShape(12.dp)),
                            contentAlignment = Alignment.Center,
                        ) {
                            Text(
                                c?.toString() ?: "",
                                fontFamily = Mono,
                                fontSize = 26.sp,
                                fontWeight = FontWeight.SemiBold,
                                color = MaterialTheme.colorScheme.onSurface,
                            )
                        }
                    }
                }
            }
        },
    )
}

/** 编辑一行文字的对话框。`validate` 返回错误信息（null 表示可以保存） */
@Composable
fun EditTextDialog(
    title: String,
    initial: String,
    onDismiss: () -> Unit,
    onSave: (String) -> Unit,
    label: String? = null,
    supporting: String? = null,
    placeholder: String? = null,
    validate: (String) -> String? = { null },
    singleLine: Boolean = true,
    keyboardType: KeyboardType = KeyboardType.Text,
) {
    var text by remember { mutableStateOf(TextFieldValue(initial, TextRange(initial.length))) }
    val error = validate(text.text)
    val focus = remember { FocusRequester() }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            Column {
                OutlinedTextField(
                    value = text,
                    onValueChange = { text = it },
                    label = label?.let { { Text(it) } },
                    placeholder = placeholder?.let { { Text(it) } },
                    singleLine = singleLine,
                    isError = error != null,
                    supportingText = (error ?: supporting)?.let { { Text(it) } },
                    keyboardOptions = KeyboardOptions(keyboardType = keyboardType, imeAction = ImeAction.Done),
                    keyboardActions = KeyboardActions(onDone = { if (error == null) onSave(text.text) }),
                    modifier = Modifier.fillMaxWidth().focusRequester(focus),
                )
            }
            // 打开时让输入框拿到焦点（弹出键盘）。要放在对话框自己的内容里：对话框是另一个窗口，它的内容挂上之前
            // focus 还没有对应的输入框，在外面请求的话 Compose 1.7 直接抛 IllegalStateException
            // （FocusRequester is not initialized），应用闪退。标准 Android 上对话框的窗口一般赶在前面挂上，
            // 别的系统上不一定（怀疑是第一次连接、弹出配对码对话框时偶尔闪退的原因）
            LaunchedEffect(Unit) { runCatching { focus.requestFocus() } }
        },
        confirmButton = {
            TextButton(onClick = { onSave(text.text) }, enabled = error == null) { Text(stringResource(R.string.save)) }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.cancel)) } },
    )
}

@Composable
fun ConfirmDialog(
    title: String,
    text: String,
    confirm: String,
    onDismiss: () -> Unit,
    onConfirm: () -> Unit,
    destructive: Boolean = false,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = { Text(text) },
        confirmButton = {
            TextButton(onClick = onConfirm) {
                Text(confirm, color = if (destructive) MaterialTheme.colorScheme.error else Color.Unspecified)
            }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.cancel)) } },
    )
}

/** 单选列表对话框 */
@Composable
fun <T> ChoiceDialog(
    title: String,
    options: List<T>,
    selected: T,
    label: @Composable (T) -> Unit,
    onDismiss: () -> Unit,
    onSelect: (T) -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            Column {
                options.forEach { o ->
                    Row(
                        Modifier
                            .fillMaxWidth()
                            .clip(RoundedCornerShape(12.dp))
                            .clickable { onSelect(o) }
                            .padding(vertical = 10.dp, horizontal = 4.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        androidx.compose.material3.RadioButton(selected = o == selected, onClick = { onSelect(o) })
                        Spacer(Modifier.width(8.dp))
                        Box(Modifier.weight(1f)) { label(o) }
                    }
                }
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.close)) } },
    )
}


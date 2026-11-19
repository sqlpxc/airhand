package app.airhand.android.ui.session

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectDragGestures
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
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
import androidx.compose.material.icons.automirrored.outlined.Undo
import androidx.compose.material.icons.outlined.Add
import androidx.compose.material.icons.outlined.Close
import androidx.compose.material.icons.outlined.ContentCopy
import androidx.compose.material.icons.outlined.DeleteOutline
import androidx.compose.material.icons.outlined.DragIndicator
import androidx.compose.material.icons.outlined.Edit
import androidx.compose.material.icons.outlined.MoreVert
import androidx.compose.material.icons.outlined.Tune
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Slider
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.VerticalDivider
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import app.airhand.android.AirhandApp
import app.airhand.android.R
import app.airhand.android.data.GameBind
import app.airhand.android.data.GameControl
import app.airhand.android.data.GameKey
import app.airhand.android.data.GameLayout
import app.airhand.android.data.GameStick
import app.airhand.android.data.GameTemplate
import app.airhand.android.data.KeyMode
import app.airhand.android.ui.ConfirmDialog
import app.airhand.android.ui.EditTextDialog
import kotlin.math.ln
import kotlin.math.pow
import kotlin.math.roundToInt

/*
 * 游戏按键的界面：菜单里的"游戏按键"页（开关、选方案、新建、编辑）、编辑方案时盖在画面上的工具栏和设置面板、
 * 选键的面板、第一次打开时的说明。按钮本身由 GameLayer 画，触摸由 GameControls / LayoutEditor 处理
 */

@Composable
private fun strings(): (Int) -> String {
    val ctx = LocalContext.current
    return remember(ctx) { { id: Int -> ctx.getString(id) } }
}

// ───────────────────────────── 菜单里的"游戏按键"页 ─────────────────────────────

/**
 * 开关（记在这台设备上）、方案列表（点一下就换上并收起菜单）、每个方案的重命名 / 复制 / 删除、新建、编辑。
 * [onEdit]：编辑这个方案（收起菜单、进入编辑）
 */
@Composable
fun GamePage(code: String, onEdit: (GameLayout) -> Unit, onClose: () -> Unit) {
    val games = AirhandApp.instance.games
    val data by games.flow.collectAsState()
    val text = strings()
    val current = data.layoutFor(code)
    val on = data.isOn(code)
    var renaming by remember { mutableStateOf<GameLayout?>(null) }
    var deleting by remember { mutableStateOf<GameLayout?>(null) }
    var creating by remember { mutableStateOf(false) }

    Row(
        Modifier
            .fillMaxWidth()
            .clickable { games.setDevice(code, on = !on, layout = current?.id) }
            .padding(horizontal = 24.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(stringResource(R.string.game_show), style = MaterialTheme.typography.bodyLarge)
            Text(stringResource(R.string.game_show_desc), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Spacer(Modifier.width(16.dp))
        Switch(checked = on && current != null, enabled = current != null, onCheckedChange = { games.setDevice(code, on = it, layout = current?.id) })
    }

    Text(
        stringResource(R.string.game_layouts),
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.padding(start = 24.dp, end = 24.dp, top = 16.dp, bottom = 4.dp),
    )
    if (data.layouts.isEmpty()) {
        Text(
            stringResource(R.string.game_no_layouts),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(horizontal = 24.dp, vertical = 8.dp),
        )
    }
    data.layouts.forEach { l ->
        LayoutRow(
            l,
            selected = l.id == current?.id,
            summary = GameKeys.summary(l, text),
            onSelect = {
                // 选了就是要用：打开并收起菜单，马上能试
                games.setDevice(code, on = true, layout = l.id)
                onClose()
            },
            onRename = { renaming = l },
            onDuplicate = { games.duplicate(l) },
            onDelete = { deleting = l },
        )
    }
    Row(Modifier.padding(horizontal = 20.dp, vertical = 12.dp), horizontalArrangement = Arrangement.spacedBy(10.dp)) {
        OutlinedButton(onClick = { creating = true }) {
            Icon(Icons.Outlined.Add, null, Modifier.size(18.dp))
            Spacer(Modifier.width(6.dp))
            Text(stringResource(R.string.game_new))
        }
        Button(onClick = { current?.let(onEdit) }, enabled = current != null) {
            Icon(Icons.Outlined.Edit, null, Modifier.size(18.dp))
            Spacer(Modifier.width(6.dp))
            Text(stringResource(R.string.game_edit))
        }
    }

    renaming?.let { l ->
        EditTextDialog(
            title = stringResource(R.string.game_rename),
            initial = l.name,
            label = stringResource(R.string.game_layout_name),
            validate = { if (it.isBlank()) "" else null },
            onDismiss = { renaming = null },
            onSave = {
                games.rename(l.id, it)
                renaming = null
            },
        )
    }
    deleting?.let { l ->
        ConfirmDialog(
            title = stringResource(R.string.game_delete_title, l.name),
            text = stringResource(R.string.game_delete_text),
            confirm = stringResource(R.string.delete),
            destructive = true,
            onDismiss = { deleting = null },
            onConfirm = {
                games.delete(l.id)
                deleting = null
            },
        )
    }
    if (creating) {
        TemplateDialog(onDismiss = { creating = false }) { t ->
            creating = false
            val l = games.create(t)
            games.setDevice(code, on = true, layout = l.id)
            // 新建的方案多半要改：直接进入编辑
            onEdit(l)
        }
    }
}

@Composable
private fun LayoutRow(
    l: GameLayout,
    selected: Boolean,
    summary: String,
    onSelect: () -> Unit,
    onRename: () -> Unit,
    onDuplicate: () -> Unit,
    onDelete: () -> Unit,
) {
    var menu by remember { mutableStateOf(false) }
    Row(
        Modifier
            .fillMaxWidth()
            .clickable(onClick = onSelect)
            .padding(start = 16.dp, end = 8.dp, top = 4.dp, bottom = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        RadioButton(selected = selected, onClick = null)
        Spacer(Modifier.width(12.dp))
        Column(Modifier.weight(1f)) {
            Text(l.name, style = MaterialTheme.typography.bodyLarge, maxLines = 1, overflow = TextOverflow.Ellipsis)
            Text(
                summary.ifEmpty { stringResource(R.string.game_empty_layout) },
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        Box {
            IconButton(onClick = { menu = true }) { Icon(Icons.Outlined.MoreVert, stringResource(R.string.more)) }
            DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                DropdownMenuItem(text = { Text(stringResource(R.string.game_rename)) }, onClick = { menu = false; onRename() })
                DropdownMenuItem(text = { Text(stringResource(R.string.game_duplicate)) }, onClick = { menu = false; onDuplicate() })
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.delete), color = MaterialTheme.colorScheme.error) },
                    onClick = { menu = false; onDelete() },
                )
            }
        }
    }
}

/** 新建方案：从哪个模板开始 */
@Composable
private fun TemplateDialog(onDismiss: () -> Unit, onPick: (GameTemplate) -> Unit) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.game_new_title)) },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState())) {
                GameTemplate.entries.forEach { t ->
                    Column(
                        Modifier
                            .fillMaxWidth()
                            .clip(RoundedCornerShape(12.dp))
                            .clickable { onPick(t) }
                            .padding(horizontal = 8.dp, vertical = 10.dp),
                    ) {
                        Text(stringResource(t.title), style = MaterialTheme.typography.bodyLarge)
                        Text(stringResource(t.desc), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                }
            }
        },
        confirmButton = {},
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.cancel)) } },
    )
}

// ───────────────────────────── 编辑方案 ─────────────────────────────

private sealed interface Pick {
    data object Add : Pick
    data class Replace(val id: Int, val binds: List<String>) : Pick
}

/**
 * 编辑方案时盖在画面上的东西：上面一条工具栏（撤销、添加、方案设置、取消、完成），
 * 选中按钮时它的设置面板放在按钮的另一边（不挡住它），什么都没选时一行提示。
 * 工具栏挡住了按钮时可以按着左边的把手拖开；提示不挡触摸。
 * 拖动按钮、点选、捏合调大小由 [LayoutEditor] 在画面上处理
 */
@Composable
fun GameEditOverlay(ed: LayoutEditor, onCancel: () -> Unit, onDone: () -> Unit) {
    var pick by remember { mutableStateOf<Pick?>(null) }
    var discard by remember { mutableStateOf(false) }
    val cancel = { if (ed.dirty) discard = true else onCancel() }
    BackHandler { cancel() }

    BoxWithConstraints(Modifier.fillMaxSize().windowInsetsPadding(WindowInsets.safeDrawing)) {
        val sel = ed.selected?.let { ed.layout.control(it) }
        // 面板放在选中的按钮的另一边；选中时定下来，拖着按钮过中线也不跳
        val onLeft = remember(ed.selected) { sel != null && sel.x > 0.5f }
        when {
            sel != null -> SidePanel(onLeft) {
                ControlPanel(ed, sel, onPick = { pick = Pick.Replace(sel.id, (sel as? GameKey)?.binds ?: emptyList()) })
            }
            ed.settings -> SidePanel(left = false) { LayoutPanel(ed) }
        }

        // 工具栏（和下面的提示）：默认在上方正中，可以拖开；不会拖出屏幕
        var offset by remember { mutableStateOf(Offset.Zero) }
        var size by remember { mutableStateOf(IntSize.Zero) }
        val maxW = constraints.maxWidth.toFloat()
        val maxH = constraints.maxHeight.toFloat()
        val top = with(LocalDensity.current) { 8.dp.toPx() }
        val drag by rememberUpdatedState { d: Offset ->
            val sx = ((maxW - size.width) / 2).coerceAtLeast(0f)
            offset = Offset(
                (offset.x + d.x).coerceIn(-sx, sx),
                (offset.y + d.y).coerceIn(-top, (maxH - size.height - top).coerceAtLeast(-top)),
            )
        }
        Column(
            Modifier
                .align(Alignment.TopCenter)
                .padding(top = 8.dp)
                .offset { IntOffset(offset.x.roundToInt(), offset.y.roundToInt()) }
                .onSizeChanged { size = it },
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            EditToolbar(ed, onAdd = { pick = Pick.Add }, onCancel = cancel, onDone = onDone, onDrag = { drag(it) })
            if (sel == null && !ed.settings) {
                // 不用 Surface：提示下面的按钮照样按得到
                Text(
                    stringResource(R.string.game_edit_hint, ed.layout.name),
                    color = Color.White,
                    style = MaterialTheme.typography.bodySmall,
                    modifier = Modifier
                        .padding(top = 8.dp)
                        .clip(RoundedCornerShape(16.dp))
                        .background(Color.Black.copy(alpha = 0.6f))
                        .padding(horizontal = 14.dp, vertical = 6.dp),
                )
            }
        }
    }

    when (val p = pick) {
        null -> {}
        Pick.Add -> KeyPicker(
            initial = emptyList(),
            onStick = { keys ->
                pick = null
                ed.add { id -> GameStick(id, 0.5f, 0.5f, keys = keys) }
            },
            onPick = { binds ->
                pick = null
                ed.add { id -> GameKey(id, 0.5f, 0.5f, binds = binds) }
            },
            onDismiss = { pick = null },
        )
        is Pick.Replace -> KeyPicker(
            initial = p.binds,
            onStick = null,
            onPick = { binds ->
                pick = null
                ed.update(p.id) { (it as? GameKey)?.copy(binds = binds) ?: it }
            },
            onDismiss = { pick = null },
        )
    }
    if (discard) {
        ConfirmDialog(
            title = stringResource(R.string.game_discard_title),
            text = stringResource(R.string.game_discard_text),
            confirm = stringResource(R.string.game_discard),
            destructive = true,
            onDismiss = { discard = false },
            onConfirm = {
                discard = false
                onCancel()
            },
        )
    }
}

@Composable
private fun EditToolbar(ed: LayoutEditor, onAdd: () -> Unit, onCancel: () -> Unit, onDone: () -> Unit, onDrag: (Offset) -> Unit) {
    Surface(
        shape = RoundedCornerShape(28.dp),
        color = MaterialTheme.colorScheme.surfaceContainerHigh.copy(alpha = 0.96f),
        shadowElevation = 6.dp,
    ) {
        Row(
            Modifier
                .horizontalScroll(rememberScrollState())
                .padding(end = 6.dp, top = 4.dp, bottom = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            // 把手：按着拖动整条工具栏（挡住按钮时挪开）
            Box(
                Modifier
                    .size(width = 32.dp, height = 48.dp)
                    .pointerInput(Unit) {
                        detectDragGestures { change, amount ->
                            change.consume()
                            onDrag(amount)
                        }
                    },
                contentAlignment = Alignment.Center,
            ) {
                Icon(Icons.Outlined.DragIndicator, stringResource(R.string.game_move_toolbar), tint = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            IconButton(onClick = ed::undo, enabled = ed.canUndo) {
                Icon(Icons.AutoMirrored.Outlined.Undo, stringResource(R.string.undo))
            }
            TextButton(onClick = onAdd) {
                Icon(Icons.Outlined.Add, null, Modifier.size(18.dp))
                Spacer(Modifier.width(4.dp))
                Text(stringResource(R.string.game_add))
            }
            TextButton(onClick = {
                ed.selected = null
                ed.settings = !ed.settings
            }) {
                Icon(Icons.Outlined.Tune, null, Modifier.size(18.dp))
                Spacer(Modifier.width(4.dp))
                Text(stringResource(R.string.game_layout_settings))
            }
            VerticalDivider(Modifier.height(24.dp).padding(horizontal = 4.dp))
            TextButton(onClick = onCancel) { Text(stringResource(R.string.cancel)) }
            Button(onClick = onDone, contentPadding = PaddingValues(horizontal = 20.dp)) { Text(stringResource(R.string.game_done)) }
        }
    }
}

/** 侧边的设置面板（挡住触摸，不会点到后面的按钮） */
@Composable
private fun BoxScope.SidePanel(left: Boolean, content: @Composable ColumnScope.() -> Unit) {
    Surface(
        shape = RoundedCornerShape(20.dp),
        color = MaterialTheme.colorScheme.surfaceContainerHigh.copy(alpha = 0.97f),
        shadowElevation = 8.dp,
        modifier = Modifier
            .align(if (left) Alignment.CenterStart else Alignment.CenterEnd)
            .padding(top = 64.dp, bottom = 8.dp, start = 8.dp, end = 8.dp)
            .width(300.dp),
    ) {
        Column(
            Modifier
                .verticalScroll(rememberScrollState())
                .padding(start = 16.dp, end = 8.dp, top = 4.dp, bottom = 12.dp),
            content = content,
        )
    }
}

@Composable
private fun PanelHeader(title: String, onClose: () -> Unit) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Text(title, style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
        IconButton(onClick = onClose) { Icon(Icons.Outlined.Close, stringResource(R.string.close)) }
    }
}

@Composable
private fun FieldLabel(text: String, trailing: String? = null) {
    Row(Modifier.padding(top = 10.dp, bottom = 2.dp, end = 8.dp), verticalAlignment = Alignment.CenterVertically) {
        Text(text, style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.weight(1f))
        trailing?.let { Text(it, style = MaterialTheme.typography.labelLarge) }
    }
}

@Composable
private fun SwitchRow(title: String, desc: String?, checked: Boolean, onChange: (Boolean) -> Unit) {
    Row(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(12.dp))
            .clickable { onChange(!checked) }
            .padding(top = 8.dp, bottom = 8.dp, end = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(title, style = MaterialTheme.typography.bodyMedium)
            desc?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant) }
        }
        Spacer(Modifier.width(8.dp))
        Switch(checked = checked, onCheckedChange = onChange)
    }
}

/** 一行"名字：值"，点一下改 */
@Composable
private fun ValueRow(title: String, value: String, onClick: () -> Unit) {
    Row(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(12.dp))
            .clickable(onClick = onClick)
            .padding(top = 10.dp, bottom = 10.dp, end = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(title, style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.onSurfaceVariant)
            Text(value, style = MaterialTheme.typography.bodyLarge, maxLines = 1, overflow = TextOverflow.Ellipsis)
        }
        Icon(Icons.Outlined.Edit, null, Modifier.size(18.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}

/** 滑块：拖一次（不管拖了多少下）只算一步撤销 */
@Composable
private fun EditSlider(ed: LayoutEditor, key: Any, value: Float, range: ClosedFloatingPointRange<Float>, onChange: (Float) -> Unit) {
    var sliding by remember(key) { mutableStateOf(false) }
    Slider(
        value = value.coerceIn(range),
        onValueChange = {
            if (!sliding) {
                ed.snapshot()
                sliding = true
            }
            onChange(it)
        },
        onValueChangeFinished = { sliding = false },
        valueRange = range,
        modifier = Modifier.padding(end = 8.dp),
    )
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun ColumnScope.ControlPanel(ed: LayoutEditor, c: GameControl, onPick: () -> Unit) {
    val text = strings()
    var labeling by remember(c.id) { mutableStateOf(false) }
    PanelHeader(stringResource(if (c is GameStick) R.string.game_stick else R.string.game_key)) { ed.selected = null }
    when (c) {
        is GameKey -> {
            FieldLabel(stringResource(R.string.game_sends))
            Row(verticalAlignment = Alignment.CenterVertically) {
                FlowRow(Modifier.weight(1f), horizontalArrangement = Arrangement.spacedBy(4.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    c.binds.forEachIndexed { i, b ->
                        if (i > 0) Text("+", modifier = Modifier.align(Alignment.CenterVertically))
                        KeyCap(GameKeys.label(b, text), selected = true) { onPick() }
                    }
                }
                TextButton(onClick = onPick) { Text(stringResource(R.string.game_change)) }
            }
            ValueRow(stringResource(R.string.game_label), c.label.ifBlank { stringResource(R.string.game_label_auto, GameKeys.label(c.binds, text)) }) {
                labeling = true
            }
            FieldLabel(stringResource(R.string.game_mode))
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                FilterChip(
                    selected = c.mode == KeyMode.HOLD,
                    onClick = { ed.update(c.id) { (it as GameKey).copy(mode = KeyMode.HOLD) } },
                    label = { Text(stringResource(R.string.game_mode_hold)) },
                )
                FilterChip(
                    selected = c.mode == KeyMode.TOGGLE,
                    onClick = { ed.update(c.id) { (it as GameKey).copy(mode = KeyMode.TOGGLE) } },
                    label = { Text(stringResource(R.string.game_mode_toggle)) },
                )
            }
            SwitchRow(stringResource(R.string.game_look), stringResource(R.string.game_look_desc), c.look) { v ->
                ed.update(c.id) { (it as GameKey).copy(look = v) }
            }
            FieldLabel(stringResource(R.string.game_size))
            EditSlider(ed, c.id, c.size, GameKey.MIN..GameKey.MAX) { v -> ed.update(c.id, record = false) { it.sized(v) } }
        }
        is GameStick -> {
            FieldLabel(stringResource(R.string.game_stick_keys))
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                FilterChip(
                    selected = c.keys == GameBind.WASD,
                    onClick = { ed.update(c.id) { (it as GameStick).copy(keys = GameBind.WASD) } },
                    label = { Text("W A S D") },
                )
                FilterChip(
                    selected = c.keys == GameBind.ARROWS,
                    onClick = { ed.update(c.id) { (it as GameStick).copy(keys = GameBind.ARROWS) } },
                    label = { Text("↑ ← ↓ →") },
                )
            }
            FieldLabel(stringResource(R.string.game_sprint))
            FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                FilterChip(
                    selected = c.sprint == null,
                    onClick = { ed.update(c.id) { (it as GameStick).copy(sprint = null) } },
                    label = { Text(stringResource(R.string.game_sprint_none)) },
                )
                GameStick.SPRINT_KEYS.forEach { k ->
                    FilterChip(
                        selected = c.sprint == k,
                        onClick = { ed.update(c.id) { (it as GameStick).copy(sprint = k) } },
                        label = { Text(GameKeys.label(k, text)) },
                    )
                }
            }
            SwitchRow(stringResource(R.string.game_floating), stringResource(R.string.game_floating_desc), c.floating) { v ->
                ed.update(c.id) { (it as GameStick).copy(floating = v) }
            }
            FieldLabel(stringResource(R.string.game_size))
            EditSlider(ed, c.id, c.size, GameStick.MIN..GameStick.MAX) { v -> ed.update(c.id, record = false) { it.sized(v) } }
        }
    }
    Row(Modifier.padding(top = 4.dp, end = 8.dp), verticalAlignment = Alignment.CenterVertically) {
        TextButton(onClick = { ed.duplicate(c.id) }) {
            Icon(Icons.Outlined.ContentCopy, null, Modifier.size(18.dp))
            Spacer(Modifier.width(6.dp))
            Text(stringResource(R.string.game_duplicate))
        }
        Spacer(Modifier.weight(1f))
        TextButton(onClick = { ed.delete(c.id) }, colors = ButtonDefaults.textButtonColors(contentColor = MaterialTheme.colorScheme.error)) {
            Icon(Icons.Outlined.DeleteOutline, null, Modifier.size(18.dp))
            Spacer(Modifier.width(6.dp))
            Text(stringResource(R.string.delete))
        }
    }

    if (labeling && c is GameKey) {
        EditTextDialog(
            title = stringResource(R.string.game_label),
            initial = c.label,
            placeholder = GameKeys.label(c.binds, text),
            supporting = stringResource(R.string.game_label_hint, GameKey.MAX_LABEL),
            validate = { if (it.trim().length > GameKey.MAX_LABEL) "" else null },
            onDismiss = { labeling = false },
            onSave = { v ->
                ed.update(c.id) { (it as GameKey).copy(label = v.trim()) }
                labeling = false
            },
        )
    }
}

/** 转视角灵敏度的滑块按对数走：0.25× … 1× … 4×，1× 在正中间 */
private fun lookToSlider(v: Float) = ln(v / GameLayout.MIN_LOOK) / ln(GameLayout.MAX_LOOK / GameLayout.MIN_LOOK)

private fun sliderToLook(p: Float): Float {
    val v = GameLayout.MIN_LOOK * (GameLayout.MAX_LOOK / GameLayout.MIN_LOOK).pow(p)
    return (v * 20).roundToInt() / 20f
}

@Composable
private fun ColumnScope.LayoutPanel(ed: LayoutEditor) {
    val l = ed.layout
    var renaming by remember { mutableStateOf(false) }
    PanelHeader(stringResource(R.string.game_layout_settings)) { ed.settings = false }
    ValueRow(stringResource(R.string.game_layout_name), l.name) { renaming = true }
    FieldLabel(stringResource(R.string.game_opacity), "${(l.opacity * 100).roundToInt()}%")
    EditSlider(ed, "opacity", l.opacity, GameLayout.MIN_OPACITY..1f) { v -> ed.change(record = false) { it.copy(opacity = v) } }
    FieldLabel(stringResource(R.string.game_look_speed), "%.2f×".format(l.look))
    EditSlider(ed, "look", lookToSlider(l.look), 0f..1f) { p -> ed.change(record = false) { it.copy(look = sliderToLook(p)) } }
    Text(
        stringResource(R.string.game_look_speed_desc),
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.padding(end = 8.dp),
    )
    SwitchRow(stringResource(R.string.game_haptics), null, l.haptics) { v -> ed.change { it.copy(haptics = v) } }

    if (renaming) {
        EditTextDialog(
            title = stringResource(R.string.game_rename),
            initial = l.name,
            label = stringResource(R.string.game_layout_name),
            validate = { if (it.isBlank()) "" else null },
            onDismiss = { renaming = false },
            onSave = { v ->
                ed.change { it.copy(name = v.trim()) }
                renaming = false
            },
        )
    }
}

// ───────────────────────────── 选键 ─────────────────────────────

/**
 * 选键：平时点一个键就选好了；打开"组合键"之后可以按顺序点最多 3 个（一起按下，例如 Ctrl+1），再点"确定"。
 * [onStick] 不为 null 时（添加）最上面还有摇杆
 */
@OptIn(ExperimentalMaterial3Api::class, ExperimentalLayoutApi::class)
@Composable
private fun KeyPicker(
    initial: List<String>,
    onStick: ((List<String>) -> Unit)?,
    onPick: (List<String>) -> Unit,
    onDismiss: () -> Unit,
) {
    val text = strings()
    var combo by remember { mutableStateOf(initial.size > 1) }
    val chosen = remember { mutableStateListOf<String>().apply { if (initial.size > 1) addAll(initial) } }
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true),
        containerColor = MaterialTheme.colorScheme.surfaceContainer,
    ) {
        Column(
            Modifier
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 20.dp)
                .padding(bottom = 24.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    stringResource(if (onStick != null) R.string.game_add_title else R.string.game_pick_title),
                    style = MaterialTheme.typography.titleMedium,
                    modifier = Modifier.weight(1f),
                )
                FilterChip(
                    selected = combo,
                    onClick = {
                        combo = !combo
                        if (!combo) chosen.clear()
                    },
                    label = { Text(stringResource(R.string.game_pick_combo)) },
                )
            }
            if (combo) {
                Text(
                    stringResource(R.string.game_pick_combo_desc, GameBind.MAX_BINDS),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Row(Modifier.padding(top = 8.dp), verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        if (chosen.isEmpty()) "—" else GameKeys.label(chosen, text),
                        style = MaterialTheme.typography.titleLarge,
                        modifier = Modifier.weight(1f),
                    )
                    TextButton(onClick = { chosen.clear() }, enabled = chosen.isNotEmpty()) { Text(stringResource(R.string.clear)) }
                    Button(onClick = { onPick(chosen.toList()) }, enabled = chosen.isNotEmpty()) { Text(stringResource(R.string.game_pick_ok)) }
                }
            }
            if (onStick != null && !combo) {
                GroupTitle(stringResource(R.string.game_stick))
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    KeyCap(stringResource(R.string.game_stick) + " · WASD") { onStick(GameBind.WASD) }
                    KeyCap(stringResource(R.string.game_stick) + " · ↑←↓→") { onStick(GameBind.ARROWS) }
                }
            }
            GameKeys.groups.forEach { g ->
                GroupTitle(stringResource(g.title))
                FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    g.tokens.forEach { t ->
                        val i = chosen.indexOf(t)
                        KeyCap(
                            GameKeys.label(t, text),
                            selected = if (combo) i >= 0 else initial == listOf(t),
                            badge = if (combo && i >= 0) "${i + 1}" else null,
                        ) {
                            when {
                                !combo -> onPick(listOf(t))
                                i >= 0 -> chosen.removeAt(i)
                                chosen.size < GameBind.MAX_BINDS -> chosen += t
                            }
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun GroupTitle(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.labelLarge,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.padding(top = 16.dp, bottom = 6.dp),
    )
}

/** 一个键帽；[badge] 是组合键里的第几个 */
@Composable
private fun KeyCap(label: String, selected: Boolean = false, badge: String? = null, onClick: () -> Unit) {
    val c = MaterialTheme.colorScheme
    val shape = RoundedCornerShape(10.dp)
    Row(
        Modifier
            .heightIn(min = 40.dp)
            .widthIn(min = 44.dp)
            .clip(shape)
            .background(if (selected) c.primaryContainer else c.surfaceContainerHighest)
            .then(if (selected) Modifier.border(1.5.dp, c.primary, shape) else Modifier)
            .clickable(onClick = onClick)
            .padding(horizontal = 10.dp, vertical = 8.dp),
        horizontalArrangement = Arrangement.Center,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(label, style = MaterialTheme.typography.labelLarge, fontWeight = FontWeight.SemiBold)
        badge?.let {
            Spacer(Modifier.width(6.dp))
            Box(Modifier.size(18.dp).clip(CircleShape).background(c.primary), contentAlignment = Alignment.Center) {
                Text(it, style = MaterialTheme.typography.labelSmall, color = c.onPrimary)
            }
        }
    }
}

// ───────────────────────────── 第一次打开时的说明 ─────────────────────────────

@Composable
fun GameTips(onDone: () -> Unit) {
    Box(
        Modifier
            .fillMaxSize()
            .background(Color.Black.copy(alpha = 0.6f))
            .clickable(onClick = onDone),
        contentAlignment = Alignment.Center,
    ) {
        Surface(
            shape = RoundedCornerShape(24.dp),
            color = MaterialTheme.colorScheme.surfaceContainerHigh,
            modifier = Modifier.widthIn(max = 480.dp).padding(24.dp),
        ) {
            Column(Modifier.verticalScroll(rememberScrollState()).padding(vertical = 20.dp)) {
                Text(stringResource(R.string.game_tips_title), style = MaterialTheme.typography.titleMedium, modifier = Modifier.padding(horizontal = 24.dp))
                Spacer(Modifier.height(8.dp))
                listOf(
                    R.string.game_tip_hold to R.string.game_tip_hold_desc,
                    R.string.game_tip_stick to R.string.game_tip_stick_desc,
                    R.string.game_tip_look to R.string.game_tip_look_desc,
                    R.string.game_tip_aim to R.string.game_tip_aim_desc,
                ).forEach { (g, a) ->
                    Row(Modifier.padding(horizontal = 24.dp, vertical = 6.dp)) {
                        Text(stringResource(g), style = MaterialTheme.typography.bodyMedium, fontWeight = FontWeight.SemiBold, modifier = Modifier.width(130.dp))
                        Text(stringResource(a), style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                }
                Text(
                    stringResource(R.string.game_tip_edit),
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

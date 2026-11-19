package app.airhand.android.ui.session

import android.content.pm.ActivityInfo
import android.media.AudioManager
import android.os.Build
import android.os.Bundle
import android.view.Surface
import android.view.WindowManager
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.CloudOff
import androidx.compose.material.icons.outlined.LinkOff
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.lifecycle.lifecycleScope
import app.airhand.android.AirhandApp
import app.airhand.android.R
import app.airhand.android.controller.RemoteSession
import app.airhand.android.core.Code
import app.airhand.android.core.DisplayInfo
import app.airhand.android.ui.Messages
import app.airhand.android.ui.theme.AirhandTheme
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.filterNotNull
import kotlinx.coroutines.launch

/**
 * 远程画面（全屏）。手机上可以操作（[PhoneSession]），电视上只看画面（[TvSession]）。
 *
 * 会话本身在 [RemoteSession] 里（属于整个应用），这个界面只是显示它：旋转、弹出键盘都不会重建，
 * 切到后台时会话降到很低的帧率，回来时马上恢复。离开这个界面（确认断开）才结束会话。
 *
 * 横屏还是竖屏跟着对方的画面走，手机上可以在菜单里换（见 [ScreenOrientation]）；电视一直横着。
 */
class SessionActivity : ComponentActivity(), RemoteView.Host {
    private val app get() = AirhandApp.instance
    private var session: RemoteSession? = null
    private lateinit var view: RemoteView
    private val keyboardOpen = mutableStateOf(false)
    private val orientation = ScreenOrientation()
    private val landscape = mutableStateOf(true)

    override val keyboard = KeyboardController { events -> session?.input(events, true) }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val s = app.session.value
        if (s == null) {
            finish()
            return
        }
        session = s
        val tv = s.viewOnly || AirhandApp.isTv(this)
        volumeControlStream = AudioManager.STREAM_MUSIC
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        WindowCompat.setDecorFitsSystemWindows(window, false)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            window.attributes.layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES
        }
        WindowInsetsControllerCompat(window, window.decorView).apply {
            hide(WindowInsetsCompat.Type.systemBars())
            systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
        view = RemoteView(this, this).apply {
            interactive = !tv
            if (tv) {
                isFocusable = false
                isFocusableInTouchMode = false
                showCursor = false
            }
        }
        // 横竖屏：对方的画面，还没收到画面时用对方当前显示器的尺寸。先按已经知道的定下来，进来时不用再转一次
        if (!AirhandApp.isTv(this)) {
            remoteSize(s.videoSize.value, s.displays.value, s.currentDisplay.value)?.let { (w, h) -> orientation.remote(w, h) }
            applyOrientation()
            lifecycleScope.launch {
                combine(s.videoSize, s.displays, s.currentDisplay, ::remoteSize).filterNotNull().collect { (w, h) ->
                    orientation.remote(w, h)
                    applyOrientation()
                }
            }
        }
        // 对方复制了文本：已经写进本机剪贴板，提示一下
        lifecycleScope.launch {
            s.clipboardIn.collect { Toast.makeText(this@SessionActivity, R.string.clipboard_received, Toast.LENGTH_SHORT).show() }
        }
        setContent {
            AirhandTheme(dark = true) {
                if (tv) {
                    TvSession(s, view, onExit = ::exit)
                } else {
                    PhoneSession(s, view, keyboard, keyboardOpen, landscape.value, onRotate = ::rotate, onExit = ::exit)
                }
            }
        }
    }

    /** 对方画面的尺寸：收到的画面；还没收到时用对方当前显示器的尺寸 */
    private fun remoteSize(video: Pair<Int, Int>?, displays: List<DisplayInfo>, current: Int): Pair<Int, Int>? =
        video ?: displays.firstOrNull { it.index == current }?.let { it.width to it.height }

    private fun applyOrientation() {
        landscape.value = orientation.landscape
        requestedOrientation = if (orientation.landscape) {
            ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE
        } else {
            ActivityInfo.SCREEN_ORIENTATION_SENSOR_PORTRAIT
        }
    }

    /** 菜单里的"横屏 / 竖屏"：换到另一个方向 */
    private fun rotate() {
        orientation.toggle()
        applyOrientation()
    }

    /** 结束会话并离开 */
    fun exit() {
        session?.let { app.endSession(it) }
        finish()
    }

    override fun onStart() {
        super.onStart()
        session?.setBackground(false)
    }

    override fun onStop() {
        super.onStop()
        session?.setBackground(true)
    }

    override fun onSurface(surface: Surface?) {
        session?.setSurface(surface)
    }

    override fun onZoom(zoomed: Boolean, shownW: Float, shownH: Float) {
        session?.setZoom(zoomed, shownW, shownH)
    }

    override fun onInput(events: List<Any>, now: Boolean) {
        session?.input(events, now)
    }

    override fun onToggleKeyboard() {
        keyboardOpen.value = !keyboardOpen.value
    }
}

/**
 * 会话不在正常显示时盖在画面上的说明：正在连接、断线重连、等对方上线、已结束。
 * 返回 true 表示盖住了（画面不可操作）。
 */
@Composable
fun SessionStatus(session: RemoteSession, big: Boolean, onExit: () -> Unit): Boolean {
    val ctx = LocalContext.current
    val phase by session.phase.collectAsState()
    val video by session.videoSize.collectAsState()
    val name by session.hostName.collectAsState()
    val who = name.ifBlank { Code.format(session.code) }
    LaunchedEffect(phase) { if (phase is RemoteSession.Phase.Closed) onExit() }

    data class St(val busy: Boolean, val title: String, val detail: String?, val icon: Boolean, val ended: Boolean, val scrim: Boolean)

    val st: St? = when (val p = phase) {
        RemoteSession.Phase.Connecting -> St(true, ctx.getString(R.string.session_connecting, who), null, false, false, true)
        RemoteSession.Phase.Live -> if (video == null) St(true, ctx.getString(R.string.session_waiting_video), null, false, false, false) else null
        is RemoteSession.Phase.Reconnecting -> St(true, ctx.getString(R.string.session_reconnecting), Messages.problem(ctx, p.reason), false, false, true)
        is RemoteSession.Phase.WaitingHost -> St(true, ctx.getString(R.string.session_waiting_host, who), ctx.getString(R.string.session_waiting_host_detail), true, false, true)
        is RemoteSession.Phase.Ended -> St(false, ctx.getString(R.string.session_ended), Messages.problem(ctx, p.reason), true, true, true)
        is RemoteSession.Phase.Failed -> St(false, ctx.getString(R.string.session_ended), Messages.problem(ctx, p.reason), true, true, true)
        RemoteSession.Phase.Closed -> null
    }
    AnimatedVisibility(st != null, enter = fadeIn(), exit = fadeOut()) {
        val s = st ?: return@AnimatedVisibility
        val focus = androidx.compose.runtime.remember { FocusRequester() }
        Box(
            Modifier
                .fillMaxSize()
                .background(if (s.scrim) Color.Black.copy(alpha = 0.72f) else Color.Transparent),
            contentAlignment = Alignment.Center,
        ) {
            Column(
                Modifier
                    .widthIn(max = if (big) 640.dp else 420.dp)
                    .padding(32.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.Center,
            ) {
                when {
                    s.busy && !s.icon -> CircularProgressIndicator(Modifier.size(if (big) 56.dp else 40.dp), color = Color.White)
                    s.ended -> Icon(Icons.Outlined.LinkOff, null, tint = Color.White, modifier = Modifier.size(if (big) 64.dp else 48.dp))
                    else -> Icon(Icons.Outlined.CloudOff, null, tint = Color.White, modifier = Modifier.size(if (big) 64.dp else 48.dp))
                }
                Spacer(Modifier.height(20.dp))
                Text(
                    s.title,
                    color = Color.White,
                    style = if (big) MaterialTheme.typography.headlineMedium else MaterialTheme.typography.titleMedium,
                    textAlign = TextAlign.Center,
                )
                s.detail?.let {
                    Spacer(Modifier.height(8.dp))
                    Text(
                        it,
                        color = Color.White.copy(alpha = 0.75f),
                        style = MaterialTheme.typography.bodyMedium,
                        fontSize = if (big) 18.sp else 14.sp,
                        textAlign = TextAlign.Center,
                    )
                }
                Spacer(Modifier.height(24.dp))
                if (s.ended) {
                    Button(onClick = onExit, modifier = Modifier.focusRequester(focus), shape = RoundedCornerShape(24.dp)) {
                        Text(stringResource(R.string.back))
                    }
                } else if (s.scrim) {
                    if (phase is RemoteSession.Phase.WaitingHost) {
                        OutlinedButton(onClick = { session.retryNow() }) { Text(stringResource(R.string.retry)) }
                        Spacer(Modifier.height(8.dp))
                    }
                    OutlinedButton(onClick = onExit, modifier = Modifier.focusRequester(focus)) {
                        Text(stringResource(R.string.disconnect), color = Color.White)
                    }
                }
            }
        }
        LaunchedEffect(s.ended, s.scrim) {
            if (big && s.scrim) runCatching { focus.requestFocus() }
        }
    }
    return st != null && st.scrim
}

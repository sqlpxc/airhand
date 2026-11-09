package app.airhand.android.ui

import android.content.Context
import android.text.format.DateUtils
import androidx.annotation.StringRes
import app.airhand.android.R
import app.airhand.android.core.Notice
import app.airhand.android.core.OsKind
import app.airhand.android.core.Problem
import app.airhand.android.core.ServerState

/**
 * 把 Rust 传来的类型（Problem / Notice / ServerState）按本机语言变成文字。
 * 和桌面控制台一样：对方和服务器只发类型，界面上不会出现别的设备的语言。
 *
 * 类型 → 文案的对应关系（[problemRes] / [noticeRes]）不依赖 Context，单元测试（MessagesTest）据此检查
 * Rust 的每一种 `Problem` / `NoticeView` 都有文案，两种语言的词条和参数也一致。
 * Rust 端新增变体时要在这里补上，否则测试失败。
 */
object Messages {
    /**
     * 问题对应的文案。带 `detail` 的问题，文案里用 `%1$s` 显示它（不带的文案会忽略这个参数）。
     * 不认识的类型返回 null（显示成通用的"出错了"）
     */
    @StringRes
    fun problemRes(p: Problem): Int? = when (p.kind) {
        "serverUnreachable" -> R.string.problem_server_unreachable
        "certMismatch" -> R.string.problem_cert_mismatch
        "incompatible" -> if ((p.server ?: 0) > (p.client ?: 0)) R.string.problem_incompatible_update_app
        else R.string.problem_incompatible_update_server
        "serverLost" -> R.string.problem_server_lost
        "notConnected" -> R.string.problem_not_connected
        "invalidCode" -> R.string.problem_invalid_code
        "codeNotFound" -> R.string.problem_code_not_found
        "rateLimited" -> R.string.problem_rate_limited
        "hostBusy" -> R.string.problem_host_busy
        "serverTimeout" -> R.string.problem_server_timeout
        "hostOffline" -> R.string.problem_host_offline
        "hostKeyChanged" -> R.string.problem_host_key_changed
        "pairingRequired" -> R.string.problem_pairing_required
        "pairingFailed" -> R.string.problem_pairing_failed
        "pairingUnavailable" -> R.string.problem_pairing_unavailable
        "peerIncompatible" -> if ((p.peer ?: 0) > (p.ours ?: 0)) R.string.problem_peer_incompatible_update_app
        else R.string.problem_peer_incompatible_update_peer
        "hostEnded" -> R.string.problem_host_ended
        "hostUnresponsive" -> R.string.problem_host_unresponsive
        "kicked" -> R.string.problem_kicked
        "codeChanged" -> R.string.problem_code_changed
        "declined" -> R.string.problem_declined
        "unanswered" -> R.string.problem_unanswered
        "sessionEnded" -> R.string.problem_session_ended
        "notController" -> R.string.problem_not_controller
        "noSession" -> R.string.problem_no_session
        "noClipboardText" -> R.string.problem_no_clipboard_text
        "failed" -> R.string.problem_failed
        else -> null
    }

    fun problem(ctx: Context, p: Problem): String {
        val res = problemRes(p) ?: return ctx.getString(R.string.problem_failed, p.detail ?: p.kind)
        return ctx.getString(res, p.detail ?: "")
    }

    fun server(ctx: Context, s: ServerState): String = when (s) {
        ServerState.Connecting -> ctx.getString(R.string.server_connecting)
        ServerState.Connected -> ctx.getString(R.string.server_connected)
        is ServerState.Disconnected -> problem(ctx, s.reason)
    }

    /**
     * 被控端提示对应的文案（带 `detail` 的用 `%1$s` 显示它）。`xxxRecovered` 只用来撤下之前的提示，本身不显示，
     * 和不认识的类型一样返回 null
     */
    @StringRes
    fun noticeRes(n: Notice, os: OsKind): Int? = when (n.kind) {
        "noDisplays" -> R.string.notice_no_displays
        "wayland" -> R.string.notice_wayland
        "captureFailed" -> R.string.notice_capture_failed
        "audioFailed" -> R.string.notice_audio_failed
        "missingPermissions" -> when {
            os == OsKind.ANDROID -> R.string.notice_android_no_input
            n.screen && n.input -> R.string.notice_mac_no_screen_input
            n.input -> R.string.notice_mac_no_input
            else -> R.string.notice_mac_no_screen
        }
        else -> null
    }

    /**
     * 只看画面（电视）时要显示的提示：本来就不操作，对方没给操作权限（无障碍服务 / 辅助功能）不用提示；
     * 同时缺屏幕录制权限的只说看不到画面。返回 null 表示不显示
     */
    fun forViewer(n: Notice): Notice? = when {
        n.kind != "missingPermissions" -> n
        !n.screen -> null
        else -> n.copy(input = false)
    }

    fun notice(ctx: Context, n: Notice, os: OsKind): String {
        val res = noticeRes(n, os) ?: return n.kind
        return ctx.getString(res, n.detail ?: "")
    }

    /** "3 分钟前" 这样的相对时间 */
    fun ago(ctx: Context, t: Long): String =
        if (t <= 0) "" else DateUtils.getRelativeTimeSpanString(t, System.currentTimeMillis(), DateUtils.MINUTE_IN_MILLIS).toString()

    fun osLabel(ctx: Context, os: String): String = os.ifBlank { ctx.getString(R.string.unknown_device) }
}

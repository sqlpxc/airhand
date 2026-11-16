package app.airhand.android.host

import app.airhand.android.core.ServerState
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * 连到本机的设备：`id` 是会话 id，`key` 是它在端到端握手里的公钥（同一台设备重连也一样），
 * `safety` 是端到端加密的安全码（对方显示的是同一个），`justPaired`：这次刚用配对码配对
 */
data class Viewer(
    val id: String,
    val key: String,
    val name: String,
    val ip: String,
    val safety: String,
    val justPaired: Boolean = false,
)

/** 本机作为被控端的状态（界面观察，[HostService] 更新）。配对码在 [app.airhand.android.data.PairingStore] 里 */
data class HostStatus(
    /** 正在共享（前台服务在运行） */
    val running: Boolean = false,
    /** 服务器分配的控制码（6 位） */
    val code: String? = null,
    val server: ServerState = ServerState.Connecting,
    val viewers: List<Viewer> = emptyList(),
    /** 有人在看画面 */
    val streaming: Boolean = false,
    /** 共享停止的原因（例如在系统里停止了录屏），显示一次 */
    val stoppedReason: String? = null,
    /** 声音共享不了的原因 */
    val audioProblem: String? = null,
)

object HostState {
    private val _flow = MutableStateFlow(HostStatus())
    val flow: StateFlow<HostStatus> = _flow.asStateFlow()
    val value get() = _flow.value

    @Synchronized
    fun update(f: (HostStatus) -> HostStatus) {
        _flow.value = f(_flow.value)
    }
}

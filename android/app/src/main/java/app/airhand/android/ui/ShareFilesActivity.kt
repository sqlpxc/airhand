package app.airhand.android.ui

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.widget.Toast
import app.airhand.android.AirhandApp
import app.airhand.android.R
import app.airhand.android.core.ClipFiles
import app.airhand.android.host.HostService
import org.json.JSONArray

/**
 * 系统分享菜单里的"发给对方（Airhand）"：分享的文件放进对方的剪贴板（正在控制的电脑；没有的话是正在控制本机的设备），
 * 到对方那里粘贴时才传（见 [ClipFiles]）。没有界面，看一眼就关
 */
class ShareFilesActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val uris = sharedUris(intent)
        val app = application as AirhandApp
        // 先定下交给谁：文件描述符交出去一次就不归这里了（见 ClipFiles.offer），不能失败了再换一个
        val session = app.session.value?.takeIf { it.canOfferFiles() }
        val send: ((JSONArray) -> Boolean)? = when {
            session != null -> { files -> session.offerFiles(files) }
            HostService.canOfferFiles() -> { files -> HostService.offerFiles(files) }
            else -> null
        }
        val ok = send != null && uris.isNotEmpty() && ClipFiles.offer(this, uris, send)
        Toast.makeText(
            this,
            if (ok) R.string.clip_files_offered else R.string.clip_files_no_target,
            Toast.LENGTH_LONG,
        ).show()
        finish()
    }

    private fun sharedUris(intent: Intent): List<Uri> = when (intent.action) {
        Intent.ACTION_SEND -> listOfNotNull(
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
            } else {
                @Suppress("DEPRECATION")
                intent.getParcelableExtra(Intent.EXTRA_STREAM)
            },
        )
        Intent.ACTION_SEND_MULTIPLE ->
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java).orEmpty()
            } else {
                @Suppress("DEPRECATION")
                intent.getParcelableArrayListExtra<Uri>(Intent.EXTRA_STREAM).orEmpty()
            }
        else -> emptyList()
    }
}

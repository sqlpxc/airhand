package app.airhand.android.core

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.ClipData
import android.content.ClipboardManager
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.provider.OpenableColumns
import android.util.Log
import androidx.core.app.NotificationCompat
import app.airhand.android.R
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.io.OutputStream
import java.util.concurrent.ConcurrentHashMap

/**
 * 剪贴板里的文件（见 Rust 端的 `airhand_link::files`、`android/native/src/files.rs`）：复制时只发清单，粘贴时才传。
 *
 * * 对方复制的文件（[apply]）：放进手机的剪贴板（content URI，[ClipFilesProvider] 提供：别的应用读的时候才经 Rust
 *   向对方取），再弹一条通知，可以"保存到下载"（文件夹也行；剪贴板里放不了文件夹，只能保存）；
 * * 手机上的文件：分享给 Airhand（[ShareFilesActivity]），公告给正在控制的电脑、正在控制本机的设备，
 *   它们粘贴时才来取（[offer]）。只有文件，分享菜单给不了文件夹。
 */
object ClipFiles {
    private const val TAG = "ClipFiles"
    private const val CHANNEL = "clipfiles"
    private const val NOTIFICATION = 7301
    const val ACTION_SAVE = "app.airhand.android.SAVE_CLIP_FILES"
    const val EXTRA_KEY = "key"
    private const val CHUNK = 1 shl 20

    /** 一项（[FileEntry] 的 JSON） */
    data class Entry(val path: List<String>, val kind: String, val size: Long, val mtime: Long) {
        val name get() = path.last()

        companion object {
            fun parse(o: JSONObject): Entry {
                val p = o.optJSONArray("path") ?: JSONArray()
                return Entry(
                    (0 until p.length()).map { p.getString(it) },
                    o.optString("kind"),
                    o.optLong("size"),
                    if (o.isNull("mtime")) -1 else o.optLong("mtime", -1),
                )
            }
        }
    }

    /** 对方的公告：key → 那几项（[ClipFilesProvider] 按它回答文件名、大小） */
    private val offers = ConcurrentHashMap<Long, List<Entry>>()
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    fun entry(key: Long, path: List<String>): Entry? = offers[key]?.find { it.path == path }

    fun authority(context: Context) = "${context.packageName}.clipfiles"

    fun uri(context: Context, key: Long, e: Entry): Uri =
        Uri.Builder().scheme("content").authority(authority(context)).appendPath(key.toString())
            .apply { for (p in e.path) appendPath(p) }.build()

    /** 对方复制了文件：放进剪贴板、弹通知，告诉对方放好了 */
    fun apply(context: Context, key: Long, items: JSONArray) {
        val entries = (0 until items.length()).map { Entry.parse(items.getJSONObject(it)) }
        offers[key] = entries
        while (offers.size > 8) offers.keys.minOrNull()?.let { offers.remove(it) }
        val files = entries.filter { it.kind == "file" }
        runCatching {
            if (files.isNotEmpty()) {
                val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                val clip = ClipData.newUri(context.contentResolver, files[0].name, uri(context, key, files[0]))
                for (f in files.drop(1)) clip.addItem(ClipData.Item(uri(context, key, f)))
                cm.setPrimaryClip(clip)
            }
        }.onFailure { Log.w(TAG, "can't put the files on the clipboard", it) }
        Native.filesReady(key)
        notifyOffer(context, key, entries)
    }

    private fun notifyOffer(context: Context, key: Long, entries: List<Entry>) {
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            nm.createNotificationChannel(
                NotificationChannel(CHANNEL, context.getString(R.string.clip_files_channel), NotificationManager.IMPORTANCE_DEFAULT),
            )
        }
        val save = PendingIntent.getBroadcast(
            context,
            key.toInt(),
            Intent(context, SaveReceiver::class.java).setAction(ACTION_SAVE).putExtra(EXTRA_KEY, key),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val names = entries.take(3).joinToString("、") { it.name } + if (entries.size > 3) "…" else ""
        val n = NotificationCompat.Builder(context, CHANNEL)
            .setSmallIcon(R.drawable.ic_stat_airhand)
            .setContentTitle(context.getString(R.string.clip_files_title, entries.size))
            .setContentText(names)
            .setStyle(NotificationCompat.BigTextStyle().bigText(context.getString(R.string.clip_files_text, names)))
            .addAction(0, context.getString(R.string.clip_files_save), save)
            .setAutoCancel(true)
            .build()
        // 没有通知权限（Android 13 起要问）时发不出来：剪贴板里照样有
        runCatching { nm.notify(NOTIFICATION, n) }
    }

    /** 把对方的公告 `key` 里的东西都保存到"下载/Airhand"（文件夹里的也是）。在后台做 */
    fun save(context: Context, key: Long) {
        val entries = offers[key] ?: return
        val app = context.applicationContext
        scope.launch {
            var failed: String? = null
            for (item in entries) {
                when (item.kind) {
                    "file" -> if (!saveFile(app, key, item, item.path)) failed = item.name
                    "dir" -> {
                        val r = JSONObject(Native.filesList(key, JSONArray(item.path).toString(), true))
                        val list = r.optJSONArray("entries")
                        if (list == null) {
                            failed = item.name
                            continue
                        }
                        for (i in 0 until list.length()) {
                            val e = Entry.parse(list.getJSONObject(i))
                            if (e.kind != "file") continue
                            val full = item.path + e.path
                            if (!saveFile(app, key, e.copy(path = full), full)) failed = e.name
                        }
                    }
                }
            }
            val nm = app.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            val n = NotificationCompat.Builder(app, CHANNEL)
                .setSmallIcon(R.drawable.ic_stat_airhand)
                .setContentTitle(
                    when {
                        failed != null -> app.getString(R.string.clip_files_failed, failed)
                        Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q -> app.getString(R.string.clip_files_saved)
                        else -> app.getString(R.string.clip_files_saved_app, legacyDir(app).absolutePath)
                    },
                )
                .setAutoCancel(true)
                .build()
            runCatching { nm.notify(NOTIFICATION, n) }
        }
    }

    /** 一个文件存到"下载/Airhand/对方的路径"。阻塞 */
    private fun saveFile(context: Context, key: Long, e: Entry, path: List<String>): Boolean {
        val dirs = path.dropLast(1)
        return runCatching {
            val (out, done) = open(context, dirs, e.name)
            out.use { copy(key, path, e, it) }
            done()
            true
        }.getOrElse {
            Log.w(TAG, "saving ${e.name} failed", it)
            false
        }
    }

    /** 下载目录里的一个新文件：写的地方，写完时调用的（Android 10 起经 MediaStore，之前写进应用自己的下载目录） */
    private fun open(context: Context, dirs: List<String>, name: String): Pair<OutputStream, () -> Unit> {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            val rel = (listOf(Environment.DIRECTORY_DOWNLOADS, "Airhand") + dirs).joinToString("/")
            val values = ContentValues().apply {
                put(MediaStore.MediaColumns.DISPLAY_NAME, name)
                put(MediaStore.MediaColumns.RELATIVE_PATH, rel)
                put(MediaStore.MediaColumns.IS_PENDING, 1)
            }
            val resolver = context.contentResolver
            val uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values) ?: error("can't create $name")
            val out = resolver.openOutputStream(uri) ?: error("can't write $name")
            return Pair(out) {
                values.clear()
                values.put(MediaStore.MediaColumns.IS_PENDING, 0)
                resolver.update(uri, values, null, null)
            }
        }
        val dir = File(legacyDir(context), dirs.joinToString(File.separator)).apply { mkdirs() }
        return Pair(File(dir, name).outputStream()) {}
    }

    /** Android 9 及更早存在哪里（应用自己的下载目录，不要存储权限） */
    private fun legacyDir(context: Context) =
        File(context.getExternalFilesDir(Environment.DIRECTORY_DOWNLOADS) ?: context.filesDir, "Airhand")

    /** 把对方的文件一块一块取下来写进 out */
    private fun copy(key: Long, path: List<String>, e: Entry, out: OutputStream) {
        val f = Native.filesOpen(key, JSONArray(path).toString(), e.size, e.mtime)
        check(f != 0L) { "the offer is gone" }
        try {
            val buf = ByteArray(CHUNK)
            var offset = 0L
            while (offset < e.size) {
                val n = Native.filesRead(f, offset, buf, buf.size)
                check(n >= 0) { "reading failed ($n)" }
                if (n == 0) break
                out.write(buf, 0, n)
                offset += n
            }
            check(offset == e.size) { "short read" }
        } finally {
            Native.filesClose(f)
        }
    }

    /**
     * 手机上分享给 Airhand 的文件（content URI）：打开、交给 Rust 公告。`send` 把文件列表交给控制端 / 被控端，
     * 返回交出去了没有。不管成不成，列表里的文件描述符都归 `send`（Rust 收下了就由 Rust 关，没交到 Rust 的用
     * [closeFds] 关）：这里不再碰它们，也就不能换一个对象再交一次。打开不了的跳过
     */
    fun offer(context: Context, uris: List<Uri>, send: (JSONArray) -> Boolean): Boolean {
        val files = JSONArray()
        for (uri in uris) {
            runCatching {
                var name = uri.lastPathSegment ?: "file"
                var size = -1L
                context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE), null, null, null)?.use { c ->
                    if (c.moveToFirst()) {
                        c.getString(0)?.let { name = it }
                        if (!c.isNull(1)) size = c.getLong(1)
                    }
                }
                val pfd = context.contentResolver.openFileDescriptor(uri, "r") ?: return@runCatching
                if (size < 0) size = pfd.statSize
                if (size < 0) {
                    pfd.close()
                    return@runCatching
                }
                files.put(JSONObject().put("name", name.replace('/', '_')).put("size", size).put("fd", pfd.detachFd()))
            }.onFailure { Log.w(TAG, "can't open a shared file", it) }
        }
        if (files.length() == 0) return false
        return send(files)
    }

    /** 关掉没交到 Rust 的文件列表里的文件描述符（见 [offer]） */
    fun closeFds(files: JSONArray) {
        for (i in 0 until files.length()) {
            runCatching { android.os.ParcelFileDescriptor.adoptFd(files.getJSONObject(i).getInt("fd")).close() }
        }
    }

    /** 通知里的"保存到下载" */
    class SaveReceiver : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            if (intent.action == ACTION_SAVE) save(context, intent.getLongExtra(EXTRA_KEY, 0))
        }
    }
}

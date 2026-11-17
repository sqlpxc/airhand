package app.airhand.android.core

import android.content.ContentProvider
import android.content.ContentValues
import android.database.Cursor
import android.database.MatrixCursor
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.ParcelFileDescriptor
import android.os.ProxyFileDescriptorCallback
import android.os.storage.StorageManager
import android.provider.OpenableColumns
import android.system.ErrnoException
import android.system.OsConstants
import android.util.Log
import android.webkit.MimeTypeMap
import androidx.annotation.RequiresApi
import org.json.JSONArray
import java.io.FileNotFoundException

/**
 * 对方复制的文件（[ClipFiles.apply] 放进剪贴板的 content URI：`content://<包名>.clipfiles/<key>/<路径>`）：
 * 别的应用读的时候才经 Rust 向对方取（见 Rust 端的 `android/native/src/files.rs`）。
 *
 * Android 8 起给的是代理文件描述符（[StorageManager.openProxyFileDescriptor]）：读的一方跳着读也行，读到哪里取到哪里；
 * 之前的版本给一个管道，从头读到尾。名字、大小直接用公告里的，不用先取内容
 */
class ClipFilesProvider : ContentProvider() {
    private val thread by lazy { HandlerThread("airhand-clipfiles").apply { start() } }
    private val handler by lazy { Handler(thread.looper) }

    override fun onCreate() = true

    /** URI 是哪个公告里的哪一项 */
    private fun entry(uri: Uri): Pair<Long, ClipFiles.Entry>? {
        val segs = uri.pathSegments
        val key = segs.firstOrNull()?.toLongOrNull() ?: return null
        val e = ClipFiles.entry(key, segs.drop(1)) ?: return null
        return Pair(key, e)
    }

    override fun getType(uri: Uri): String {
        val name = uri.lastPathSegment ?: return "application/octet-stream"
        val ext = name.substringAfterLast('.', "").lowercase()
        return MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext) ?: "application/octet-stream"
    }

    override fun query(uri: Uri, projection: Array<out String>?, selection: String?, args: Array<out String>?, sort: String?): Cursor? {
        val (_, e) = entry(uri) ?: return null
        val cols = projection ?: arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE)
        val row = cols.map {
            when (it) {
                OpenableColumns.DISPLAY_NAME -> e.name
                OpenableColumns.SIZE -> e.size
                else -> null
            }
        }
        return MatrixCursor(cols, 1).apply { addRow(row) }
    }

    override fun openFile(uri: Uri, mode: String): ParcelFileDescriptor {
        if (mode != "r") throw FileNotFoundException("read only")
        val (key, e) = entry(uri) ?: throw FileNotFoundException(uri.toString())
        val file = Native.filesOpen(key, JSONArray(e.path).toString(), e.size, e.mtime)
        if (file == 0L) throw FileNotFoundException("the files on the remote clipboard are gone")
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val sm = context!!.getSystemService(StorageManager::class.java)
            return sm.openProxyFileDescriptor(ParcelFileDescriptor.MODE_READ_ONLY, Proxy(file, e.size), handler)
        }
        // Android 7：管道，从头读到尾
        val (read, write) = ParcelFileDescriptor.createReliablePipe()
        Thread {
            ParcelFileDescriptor.AutoCloseOutputStream(write).use { out ->
                val buf = ByteArray(1 shl 20)
                var offset = 0L
                try {
                    while (offset < e.size) {
                        val n = Native.filesRead(file, offset, buf, buf.size)
                        if (n <= 0) {
                            write.closeWithError("reading from the remote computer failed")
                            return@use
                        }
                        out.write(buf, 0, n)
                        offset += n
                    }
                } catch (t: Throwable) {
                    Log.w("ClipFilesProvider", "pipe failed", t)
                } finally {
                    Native.filesClose(file)
                }
            }
        }.start()
        return read
    }

    /** 代理文件描述符：读的时候才向对方取（在 [thread] 上，可以阻塞） */
    @RequiresApi(Build.VERSION_CODES.O)
    private class Proxy(private val file: Long, private val size: Long) : ProxyFileDescriptorCallback() {
        override fun onGetSize() = size

        override fun onRead(offset: Long, size: Int, data: ByteArray): Int {
            val n = Native.filesRead(file, offset, data, size)
            if (n < 0) throw ErrnoException("read", OsConstants.EIO)
            return n
        }

        override fun onRelease() = Native.filesClose(file)
    }

    override fun insert(uri: Uri, values: ContentValues?): Uri? = null
    override fun delete(uri: Uri, selection: String?, args: Array<out String>?) = 0
    override fun update(uri: Uri, values: ContentValues?, selection: String?, args: Array<out String>?) = 0
}

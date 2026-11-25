package app.airhand.android.core

import android.app.ActivityManager.RunningAppProcessInfo
import android.app.ApplicationExitInfo
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class CrashLogTest {
    @Test
    fun exportKeepsTheNewestRecords() {
        val records = listOf("newest\n", "middle", "oldest")
        assertEquals("newest\n\nmiddle\n\noldest", CrashLog.joinRecords(records, 1000))

        // 放不下的旧记录不要，说一声少了几条
        val a = "a".repeat(100)
        val b = "b".repeat(100)
        val c = "c".repeat(100)
        val two = CrashLog.joinRecords(listOf(a, b, c), 64 + 202)
        assertEquals("$a\n\n$b\n\n(1 older record left out)", two)
        val one = CrashLog.joinRecords(listOf(a, b, c), 64 + 150)
        assertEquals("$a\n\n(2 older records left out)", one)
    }

    @Test
    fun aRecordTooLongForTheLimitIsCut() {
        val huge = "x".repeat(10_000)
        val s = CrashLog.joinRecords(listOf(huge, "older"), 1000)
        assertTrue(s.length <= 1000)
        assertTrue(s.startsWith("xxx"))
        assertTrue(s, s.contains("…\n\n(1 older record left out)"))
        // 导出的总长度放得进 GitHub 的一条评论
        assertTrue(CrashLog.joinRecords(List(20) { huge }, CrashLog.MAX_EXPORT).length <= 65_536)
    }

    @Test
    fun nothingRecorded() {
        assertEquals("", CrashLog.joinRecords(emptyList(), CrashLog.MAX_EXPORT))
    }

    @Test
    fun whichExitsAreWorthRecording() {
        val fg = RunningAppProcessInfo.IMPORTANCE_FOREGROUND
        val service = RunningAppProcessInfo.IMPORTANCE_FOREGROUND_SERVICE
        val cached = RunningAppProcessInfo.IMPORTANCE_CACHED
        // 崩溃、无响应：在后台也记
        for (r in listOf(ApplicationExitInfo.REASON_CRASH, ApplicationExitInfo.REASON_CRASH_NATIVE, ApplicationExitInfo.REASON_ANR)) {
            assertTrue(CrashLog.worthRecording(r, fg))
            assertTrue(CrashLog.worthRecording(r, cached))
        }
        // 内存不够、被信号杀掉：用户看着的时候（包括正在共享屏幕的前台服务）才算闪退，在后台被回收是正常的
        for (r in listOf(ApplicationExitInfo.REASON_LOW_MEMORY, ApplicationExitInfo.REASON_SIGNALED)) {
            assertTrue(CrashLog.worthRecording(r, fg))
            assertTrue(CrashLog.worthRecording(r, service))
            assertFalse(CrashLog.worthRecording(r, cached))
        }
        // 用户自己退出、强行停止、应用自己退出、升级：不记
        for (r in listOf(
            ApplicationExitInfo.REASON_USER_REQUESTED,
            ApplicationExitInfo.REASON_USER_STOPPED,
            ApplicationExitInfo.REASON_EXIT_SELF,
            ApplicationExitInfo.REASON_PERMISSION_CHANGE,
            ApplicationExitInfo.REASON_OTHER,
            ApplicationExitInfo.REASON_UNKNOWN,
        )) {
            assertFalse(CrashLog.worthRecording(r, fg))
        }
    }
}

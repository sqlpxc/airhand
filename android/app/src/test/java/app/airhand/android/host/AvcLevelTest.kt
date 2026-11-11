package app.airhand.android.host

import android.media.MediaCodecInfo.CodecProfileLevel.AVCLevel4
import android.media.MediaCodecInfo.CodecProfileLevel.AVCLevel41
import android.media.MediaCodecInfo.CodecProfileLevel.AVCLevel42
import android.media.MediaCodecInfo.CodecProfileLevel.AVCLevel51
import android.media.MediaCodecInfo.CodecProfileLevel.AVCLevel52
import android.media.MediaCodecInfo.CodecProfileLevel.AVCLevel62
import org.junit.Assert.assertEquals
import org.junit.Test

class AvcLevelTest {
    private fun level(w: Int, h: Int, fps: Int, max: Int = AVCLevel62) = ScreenCapture.avcLevel(w, h, fps, max)

    @Test
    fun theLowestLevelThatFitsButAtLeast41() {
        // 小画面也至少 4.1（更低的级别 High 档次的码率上限只有 25 Mbps）
        assertEquals(AVCLevel41, level(1280, 720, 30))
        assertEquals(AVCLevel41, level(1920, 1088, 30))
        assertEquals(AVCLevel42, level(1920, 1088, 60))
        // 手机竖屏 1080×2400：一帧就超过了 4.2，60 帧要 5.1
        assertEquals(AVCLevel51, level(1088, 2400, 60))
        assertEquals(AVCLevel52, level(3840, 2160, 60))
    }

    @Test
    fun neverAboveWhatTheEncoderSupports() {
        assertEquals(AVCLevel4, level(1088, 2400, 60, max = AVCLevel4))
        assertEquals(AVCLevel52, level(7680, 4320, 60, max = AVCLevel52))
    }
}

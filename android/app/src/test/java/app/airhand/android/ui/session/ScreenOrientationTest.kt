package app.airhand.android.ui.session

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** 远程画面的横竖：跟着对方画面走，菜单里可以换 */
class ScreenOrientationTest {
    @Test
    fun landscapeUntilTheRemotePictureIsKnown() {
        assertTrue(ScreenOrientation().landscape)
    }

    @Test
    fun followsTheRemotePicture() {
        val o = ScreenOrientation()
        o.remote(1080, 2400)
        assertFalse(o.landscape)
        o.remote(2400, 1080)
        assertTrue(o.landscape)
        // 正方形当横的
        o.remote(1000, 1000)
        assertTrue(o.landscape)
    }

    @Test
    fun ignoresEmptySizes() {
        val o = ScreenOrientation()
        o.remote(1080, 2400)
        o.remote(0, 0)
        assertFalse(o.landscape)
    }

    @Test
    fun theMenuSwitchesToTheOtherOrientation() {
        val o = ScreenOrientation()
        o.toggle()
        assertFalse(o.landscape)
        o.toggle()
        assertTrue(o.landscape)
    }

    @Test
    fun theChoiceStaysWhileTheRemoteKeepsItsOrientation() {
        val o = ScreenOrientation()
        o.remote(1920, 1080)
        o.toggle()
        // 换画质、缩放：尺寸变了，方向没变
        o.remote(1280, 720)
        assertFalse(o.landscape)
    }

    @Test
    fun followsTheRemoteAgainWhenItRotates() {
        val o = ScreenOrientation()
        o.remote(1080, 2400)
        o.toggle()
        assertTrue(o.landscape)
        // 对方手机转成横屏：跟着对方（本来也是横的）
        o.remote(2400, 1080)
        assertTrue(o.landscape)
        // 再转回竖屏：跟着变竖，不再用之前选的横屏
        o.remote(1080, 2400)
        assertFalse(o.landscape)
    }
}

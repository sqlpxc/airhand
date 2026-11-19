package app.airhand.android.ui.session

/**
 * 远程画面用横屏还是竖屏。
 *
 * * 跟着对方的画面走：横的画面（电脑、横着的手机）横屏，竖的画面（竖着的手机、竖放的显示器）竖屏；
 *   还不知道对方画面的方向时横屏。
 * * 菜单里可以换到另一个方向；之后对方的画面换了方向（对方手机转了屏、换了显示器）时，又跟着对方走。
 */
class ScreenOrientation {
    /** 对方画面是不是横的，null 表示还不知道 */
    private var remote: Boolean? = null

    /** 用户在菜单里选的方向，null 表示跟着对方 */
    private var chosen: Boolean? = null

    /** 现在应该横屏 */
    val landscape: Boolean get() = chosen ?: remote ?: true

    /** 对方画面的尺寸（宽 × 高）。方向变了就不再用菜单里选的方向 */
    fun remote(width: Int, height: Int) {
        if (width <= 0 || height <= 0) return
        val l = width >= height
        if (l == remote) return
        remote = l
        chosen = null
    }

    /** 菜单里的"横屏 / 竖屏"：换到另一个方向 */
    fun toggle() {
        chosen = !landscape
    }
}

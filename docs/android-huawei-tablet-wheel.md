# 华为安卓平板外接鼠标滚轮兼容

Huawei MatePad 11.5S 用户通过蓝牙鼠标远控电脑时，滚轮使远端指针上下移动，电脑页面没有滚动，对应 Issue #57。本次兼容实现最初随基于 v0.4.0 的独立客户端交付；用户随后反馈问题解决，现整理为基于 main 的贡献。

## 行为

Android 原生 `ACTION_SCROLL` 已由 `RemoteView.scroll()` 处理。本修复针对另一条输入路径：部分设备将滚轮转换为触摸屏来源、`TOOL_TYPE_UNKNOWN` 的单指 DOWN/MOVE/UP 序列，原有手势识别会把这些事件用于移动指针。

`TabletWheelDecoder` 只接收 UNKNOWN 工具类型、触摸屏来源、单指、无按钮并匹配设备与 downTime 的序列。`TabletWheelCompat` 在普通手势识别前截获它，将位移转换成鼠标 `ACTION_SCROLL`，交给原有滚轮路径，因此复用滚轮方向、速度、模式、平滑滚动及网络协议处理。

- 真正的手指、触控笔、多点触摸、正常鼠标按钮不按该规则识别。
- 原生鼠标滚轮继续使用现有路径。
- 编辑布局和只读会话仍由已有入口优先处理，不向电脑发送此类输入。
- 失去焦点或进入布局编辑时清除兼容层中的序列状态。
- Huawei/Honor 默认开启兼容，其他设备可手动开启；设置独立存储，用户可随时关闭。
- 默认 48 dp 每格，可调范围 12–120 dp；这是经验初值，不是某型号的硬件标定值。

设置页和会话菜单新增“华为平板滚轮兼容”、校准滑块以及可选输入诊断。继续使用现有“反转滚轮”控制方向。

## 验证

| 范围 | 已知结果 |
| --- | --- |
| v0.4.0 独立客户端 | release 构建成功，152 项单元测试通过，Lint 0 errors / 27 warnings |
| Android 34 x86_64 模拟器 | MotionEvent → RemoteView → 协议 JSON 的设备测试通过；覆盖特殊滚轮、原生滚轮、反转、按钮、手指、禁用、编辑及只读路径 |
| MatePad 11.5S，用户所述安卓系统，蓝牙鼠标 | 用户在交付后反馈“问题解决的很好”；这是用户反馈，维护者尚未自行在该平板复测 |
| 当前 main `3261ba6` | 168 项 release 单元测试通过，Lint 0 errors / 27 warnings，debug instrumentation Kotlin 编译通过；跳过 cargoBuild，未进行完整原生构建或远控实机验收 |

尚无该用户设备的原始 MotionEvent 日志，因此不能断言所有 MatePad、系统版本或鼠标使用相同事件形态，也不能把用户反馈当作完整设备矩阵验收。FINGER 工具类型的系统合成事件不强行拦截，以免影响真实触摸。

main 检查使用 JDK 17，并通过本地 init script 将 NDK 27.2 替换为设备上已有的 27.1。不能将 v0.4.0 的原生库用于 main。

设备测试入口不依赖实时远控连接。在专用模拟器或测试设备安装 debug 与 androidTest APK 后运行：

```sh
adb shell am instrument -w app.airhand.android.debug.test/app.airhand.android.wheelpatch.WheelPatchInstrumentation
```

设备用例临时修改兼容偏好并在结束时恢复。单元测试位于 `android/app/src/test/java/app/airhand/android/wheelpatch`。

## 输入诊断

设置中打开“输入诊断”，重新进入会话，滚动鼠标，再真实手指滑动一次：

```sh
adb logcat -v time -s AirhandWheelPatch:I
```

诊断仅写本地 Android 日志，每个 RemoteView 最多记录前 120 个触摸事件，包含工具类型、来源、设备 ID、坐标和滚动轴；不会自动上传。测试结束后关闭。如某设备仍不能滚动，应先采样确认，而不是扩大规则去吞掉所有触摸。

## 许可与范围

实现以本项目 MIT 许可提供。没有复制 ToDesk 闭源代码，也没有复制 RustDesk 代码。正式集成保持上游包名、应用名称、版本、签名和协议配置，个人独立客户端的构建身份不属于本修复。

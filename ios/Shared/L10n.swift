import Foundation

/// 界面文案（中文 / 英文，跟随系统语言）。和 Android 的 strings.xml 是同一套说法
enum L {
    static let zh: Bool = (Locale.preferredLanguages.first ?? "en").lowercased().hasPrefix("zh")

    static func t(_ key: String) -> String {
        (zh ? zhTable[key] : enTable[key]) ?? enTable[key] ?? key
    }

    static func t(_ key: String, _ args: CVarArg...) -> String {
        String(format: t(key), arguments: args)
    }

    // MARK: 中文
    static let zhTable: [String: String] = [
        "cancel": "取消", "close": "关闭", "save": "保存", "delete": "删除", "done": "完成", "undo": "撤销",
        "copy": "复制", "copied": "已复制", "share": "分享", "ok": "好", "on": "开", "off": "关", "retry": "立即重试",
        "connect": "连接", "connecting": "连接中…", "disconnect": "断开", "rename": "重命名", "forget": "忘掉",
        "unknown_device": "未知设备", "hardware": "硬件", "software": "软件", "open_settings": "打开设置",
        "tab_connect": "远程", "tab_share": "共享本机", "tab_settings": "设置",
        "server_online": "已连接到服务器", "server_connecting": "正在连接服务器…", "server_offline": "未连接服务器",
        "server_not_set": "还没有设置服务器",
        "connect_title": "连接到设备", "connect_hint": "输入对方 Airhand 上显示的 6 位控制码",
        "connect_paste": "粘贴", "connect_no_code_in_clipboard": "剪贴板里没有控制码",
        "my_devices": "我的设备", "my_devices_empty": "连接成功的设备会保存在这里，下次点一下就能连",
        "device_waiting": "对方不在线 · 上线后自动连接", "copy_code": "复制控制码",
        "device_delete_title": "删除设备？", "device_delete_text": "从“我的设备”里删除“%@”。之后需要重新输入控制码才能连接。",
        "device_deleted": "已删除“%@”", "device_name_placeholder": "名称",
        "last_connected": "上次连接 %@", "session_active": "正在连接 · 点按回到远程画面", "session_live_card": "远程中 · 点按回到画面",
        "setup_server_title": "先设置服务器", "setup_server_body": "Airhand 通过你自己部署的中转服务器连接设备。在“设置”里填上服务器地址就能开始使用。",
        "go_settings": "去设置",
        // 配对
        "pairing_title": "输入配对码", "pairing_hint": "对方显示的 6 位配对码（电脑在 Airhand 托盘菜单里，手机在 Airhand 的“共享本机”页）。配对之后对方记住这台设备，下次不用再输",
        "pairing_label": "配对码", "session_safety": "安全码 %@",
        // 共享本机
        "share_intro_title": "让别人远程查看这台设备", "share_intro_body": "开始共享后，对方在 Airhand 里输入本机的控制码和配对码（配对码只在第一次要）就能看到这台设备的屏幕和声音。只把这两个码告诉信任的人。",
        "share_view_only": "iOS 不允许其他应用操作本机，所以对方只能看、不能操作。",
        "share_start": "开始共享屏幕", "share_start_hint": "点按后在系统弹窗里选择“开始直播”",
        "share_your_code": "本机控制码", "share_ready": "等待对方连接", "share_pairing": "配对码",
        "share_pairing_desc": "第一次连接本机的设备还要输入这个配对码，配对之后只用控制码就能连",
        "share_pairing_locked": "有人多次输错配对码，配对已暂停。换一个配对码就恢复", "share_pairing_new": "换一个配对码",
        "share_forget_paired": "忘掉已配对的 %d 台设备", "share_forget_paired_title": "忘掉已配对的设备？",
        "share_forget_paired_text": "它们下次连接时要重新输入配对码。正连着的设备不会被断开。",
        "share_viewers": "正在查看本机", "share_no_viewers": "还没有人连接", "share_kick_all": "全部断开",
        "share_kick_text": "断开所有正在查看本机的设备？配对过的设备之后仍可以重新连接；不想让它们再连，请忘掉已配对的设备或更换控制码。",
        "share_new_code": "更换控制码", "share_new_code_title": "更换控制码？",
        "share_new_code_text": "旧的控制码立即失效，正在连接的设备会被断开，之前保存了本机的设备也连不上了。配对码也会换一个，配对过的设备全部忘掉。",
        "share_stop": "停止共享", "share_stop_title": "停止共享？", "share_stop_text": "正在查看本机的设备会被断开。",
        "share_code_message": "用 Airhand 连接我的设备，控制码：%@，配对码（只在第一次连接时要）：%@",
        "share_connecting": "正在连接服务器…", "share_sharing": "正在共享屏幕", "share_no_group": "这个构建没有 App Group 权限，屏幕广播扩展读不到设置。请用带签名的构建。",
        "share_audio_note": "对方能听到本机正在播放的声音（不会录麦克风）。",
        "share_stopped_by_user": "已停止共享", "share_ext_title": "%@ 连上了本机", "share_ext_paired_title": "%@ 刚刚配对并连上了本机",
        "share_ext_text": "对方能看到这台设备的屏幕。来自 %@。安全码 %@：中间没有人的话，对方显示的是同一个。",
        "share_ext_failed": "无法开始共享：%@", "share_ext_no_server": "还没有设置服务器，请先在 Airhand 的设置里填写",
        "share_paired_badge": "刚配对",
        // 设置
        "settings_server": "服务器", "server_address": "服务器地址", "server_address_hint": "地址:端口，例如 relay.example.com:4433",
        "server_address_invalid": "格式应为 地址:端口", "server_fingerprint": "证书指纹", "fingerprint_none": "未设置：第一次连接时记住服务器的证书",
        "fingerprint_learned": "未设置，已记住第一次连接时的证书（%@…）",
        "fingerprint_hint": "服务器日志里打印的 SHA-256 指纹（64 位十六进制，可以带冒号）。留空则第一次连接时记住服务器的证书",
        "fingerprint_invalid": "指纹应为 64 位十六进制数字", "settings_this_device": "本机", "device_name": "设备名称",
        "device_name_hint": "连接时对方看到的名字", "settings_current_code": "当前 %@ · 更换后旧控制码立即失效",
        "settings_no_code": "还没有控制码（第一次共享时分配）", "settings_code_reset": "下次共享时会分配新的控制码",
        "settings_paired": "配对过的设备", "settings_paired_none": "还没有（第一次连接本机的设备要输入配对码）",
        "settings_paired_count": "%d 台设备只用控制码就能连本机", "settings_remote": "远程控制", "default_quality": "默认画质",
        "touch_mode": "操作方式", "mode_auto": "自动", "mode_auto_desc": "控制电脑用触控板，控制手机用触屏",
        "mode_trackpad": "触控板", "mode_trackpad_desc": "滑动移动指针，点按单击，适合精确操作电脑",
        "mode_touch": "触屏", "mode_touch_desc": "点哪里就点到哪里，像操作自己的手机",
        "play_audio": "播放对方的声音", "play_audio_desc": "连接后自动打开声音",
        "av_sync": "音画同步", "av_sync_desc": "看视频时让画面与声音对齐，会增加操作延迟；播放声音时生效",
        "nav_keys": "控制手机时显示导航键", "nav_keys_desc": "在画面下方显示返回、主页、最近任务",
        "local_cursor": "拖动窗口时显示本地光标", "local_cursor_desc": "对方把光标画进画面里时照样画本机的光标，跟手不延迟",
        "settings_data": "数据", "clear_devices": "清除已保存的设备", "clear_devices_desc": "共 %d 台",
        "clear_devices_confirm": "“我的设备”里的所有设备都会被删除。",
        "data_note": "所有数据（设置、保存的设备、本机控制码）只保存在这台设备上 Airhand 自己的目录里，不会备份，卸载时一并删除。",
        "settings_about": "关于", "version": "版本", "version_value": "%@ · 协议版本 %d", "codec": "编码格式",
        "codec_auto": "自动", "codec_auto_value": "自动（%@）", "show_tips": "手势说明",
        "server_restart_note": "保存后用新的设置重新连接服务器",
        // 画质预设
        "preset_saver": "省流", "preset_saver_desc": "720p · 24 fps · 1.2 Mbps，适合移动网络",
        "preset_balanced": "均衡", "preset_balanced_desc": "1080p · 30 fps · 4 Mbps",
        "preset_smooth": "流畅", "preset_smooth_desc": "1080p · 60 fps · 8 Mbps",
        "preset_hd": "高清", "preset_hd_desc": "1440p · 60 fps · 16 Mbps",
        "preset_native": "原画", "preset_native_desc": "原始分辨率 · 60 fps · 30 Mbps",
        "encoder_info": "%@ %d×%d（%@，%@）",
        // 远程画面
        "session_connecting": "正在连接 %@…", "session_waiting_video": "正在等待画面…", "session_reconnecting": "连接中断，正在重连…",
        "session_waiting_host": "%@ 已离线", "session_waiting_host_detail": "对方重新上线后会自动连接", "session_ended": "连接已结束",
        "disconnect_title": "断开连接？", "disconnect_text": "断开与“%@”的连接。",
        "latency_ms": "%d ms", "path_direct": "直连", "path_relayed": "中转",
        "clipboard_received": "已复制对方的剪贴板内容", "audio_playback_failed": "无法播放声音：%@",
        "menu_keyboard": "键盘", "menu_shortcuts": "快捷键", "menu_sound": "声音", "menu_mic": "麦克风",
        "menu_mouse_lock": "锁定鼠标", "menu_quality": "画质", "menu_clipboard": "剪贴板", "menu_displays": "显示器",
        "menu_fit": "适应屏幕", "menu_rotate": "旋转画面", "menu_help": "手势说明", "menu_files": "发送文件",
        "menu_game": "游戏按键", "menu_minimize": "回到首页", "menu_mode": "操作方式", "menu_more": "更多",
        "hide_keyboard": "收起键盘", "display_n": "显示器 %d", "display_primary": "主显示器",
        "clipboard_send": "同步到对方的剪贴板", "clipboard_send_desc": "把本机复制的文字发过去，对方可以粘贴",
        "clipboard_sent": "已发送到对方的剪贴板", "clipboard_type": "作为键盘输入发送", "clipboard_type_desc": "直接把本机复制的文字打到对方当前的输入框里",
        "clipboard_empty": "本机剪贴板没有文本",
        "mic_unavailable": "对方现在不能接收麦克风", "mic_busy": "另一台设备正在使用对方的麦克风", "mic_denied": "没有麦克风权限，请在系统设置里允许",
        "mic_on_toast": "麦克风已打开，对方能听到你",
        // 防窥模式（Windows、Mac 被控端）
        "privacy_toggle": "防窥模式",
        "privacy_off": "打开后，对方电脑的显示器全部黑屏、键盘鼠标不起作用，只有你看得到、操作得了（画面换成一块虚拟显示器）。断开连接后自动恢复",
        "privacy_off_mac": "打开后，对方 Mac 的显示器全部黑屏、键盘鼠标和触控板不起作用，只有你看得到、操作得了。断开连接后自动恢复",
        "privacy_starting": "正在打开：对方电脑正在换成虚拟显示器，要几秒钟", "privacy_starting_mac": "正在打开",
        "privacy_on": "已打开：对方的显示器全部黑屏、键盘鼠标不起作用。断开连接后自动恢复；断线时等你重连 30 秒，期间重连会接着保持",
        "privacy_failed": "防窥模式没能打开（%@），对方的屏幕已经恢复。可以再试一次",
        "privacy_unavailable": "对方电脑用不了：需要为所有用户安装 Airhand，并在安装时装上虚拟显示器驱动",
        "privacy_unavailable_mac": "对方 Mac 用不了：要先在那台 Mac 的“系统设置 → 隐私与安全性 → 辅助功能”里允许 Airhand（挡住它的键盘鼠标要用到）",
        "privacy_not_allowed": "对方电脑的设置不允许防窥模式（config.toml 里的 privacy_mode）",
        "privacy_note": "对方电脑前的人可以断开它的网络或者重启它来结束防窥模式",
        "view_only_host": "对方是 iPhone / iPad，只能看不能操作",
        "files_sent": "已放进对方的剪贴板，到对方那里粘贴即可", "files_received": "对方复制了 %d 项",
        "files_save": "保存到“文件”", "files_saved": "已保存到“文件 › 我的 iPhone › Airhand”", "files_failed": "%@ 没有保存成功",
        "files_saving": "正在接收…",
        "pointer_locked": "鼠标已锁定 · 长按 Esc 或在菜单里关闭",
        // 手势说明
        "tips_trackpad": "触控板模式", "tips_touch": "触屏模式", "tips_phone": "控制手机",
        "tips_menu": "画面边上的圆钮是菜单：键盘、快捷键、操作方式、画质……圆钮可以拖到别处。",
        "g_slide": "单指滑动", "g_tap": "单击", "g_long_press": "长按", "g_tap_then_drag": "点一下再按住拖", "g_two_tap": "双指点击",
        "g_two_slide": "双指拖动", "g_pinch": "双指捏合", "g_three_tap": "三指点击", "g_move_pointer": "移动指针",
        "g_left_click": "左键单击（快速点两下 = 双击）", "g_right_click": "右键", "g_click_there": "在手指处单击",
        "g_right_click_there": "在手指处右键", "g_drag": "按住左键拖动（选文字、拖窗口）", "g_scroll": "滚动（松手有惯性）",
        "g_zoom": "放大画面（指针会带着画面走）", "g_zoom_pan": "放大 / 移动画面", "g_keyboard": "打开 / 收起键盘",
        "g_same_as_phone": "直接作用在对方手机上", "g_mouse": "外接鼠标、键盘", "g_mouse_desc": "直接对应对方的鼠标、键盘；iPad 上可以锁定鼠标玩游戏",
        // 游戏按键
        "game_show": "在画面上显示", "game_show_desc": "按住按钮就是按着对应的键；其他地方照常操作，游戏隐藏了光标时拖动就是转视角",
        "game_layouts": "方案", "game_no_layouts": "还没有方案，新建一个开始吧", "game_new": "新建方案", "game_edit": "编辑布局",
        "game_new_title": "从哪个模板开始", "game_duplicate": "复制一份", "game_layout_name": "方案名称",
        "game_delete_title": "删除“%@”？", "game_copy_name": "%@ 副本",
        "game_tpl_shooter": "射击（WASD）", "game_tpl_shooter_desc": "摇杆（推出边缘奔跑）、开火（按住拖动瞄准）、开镜、跳、蹲、换弹、武器 1–3",
        "game_tpl_skills": "技能（MOBA / ARPG）", "game_tpl_skills_desc": "Q W E R、D F、物品 1–4、B、空格；点画面就是点击",
        "game_tpl_arrows": "方向键（横版 / 模拟器）", "game_tpl_arrows_desc": "方向键摇杆、Z X C、空格、回车、Esc",
        "game_tpl_blank": "空白", "game_tpl_blank_desc": "自己添加按钮",
        "game_add": "添加", "game_layout_settings": "方案设置", "game_edit_hint": "拖动摆放 · 双指捏合调大小 · 点一下修改",
        "game_discard_title": "放弃修改？", "game_discard_text": "对这个方案的修改不会保存。", "game_discard": "放弃",
        "game_key": "按钮", "game_stick": "摇杆", "game_sends": "按下", "game_change": "更改", "game_label": "按钮上的文字",
        "game_label_hint": "留空就显示键名", "game_mode": "按法", "game_mode_hold": "按住", "game_mode_toggle": "点一下切换",
        "game_look": "拖动可以转视角", "game_look_desc": "按着的时候，例如一边开火一边瞄准", "game_size": "大小",
        "game_stick_keys": "方向", "game_sprint": "推出边缘时加按", "game_sprint_none": "不加",
        "game_floating": "从按下的地方开始", "game_floating_desc": "拇指不用正好落在摇杆中间",
        "game_opacity": "不透明度", "game_look_speed": "转视角灵敏度", "game_haptics": "按下时振动",
        "game_pick_title": "选择按键", "game_pick_combo": "组合键（最多 3 个，按顺序一起按下）",
        "game_group_mouse": "鼠标", "game_group_common": "常用", "game_group_letters": "字母", "game_group_digits": "数字",
        "game_group_function": "功能键", "game_group_nav": "方向和编辑", "game_group_symbols": "符号",
        "game_mouse_left": "左键", "game_mouse_right": "右键", "game_mouse_middle": "中键", "game_wheel_up": "滚轮↑", "game_wheel_down": "滚轮↓",
        "game_duplicate_item": "复制", "game_empty_layout_hint": "还没有按钮，点上面的 + 添加", "game_tip": "按住按钮就是按着那个键；空白处拖动在游戏隐藏光标时转视角。菜单 → 游戏按键 → 编辑布局可以自己摆。",
        // 快捷键
        "combo_ctrl_alt_del": "Ctrl + Alt + Del", "combo_alt_tab": "Alt + Tab 切换窗口", "combo_win": "Win 开始菜单",
        "combo_win_d": "Win + D 显示桌面", "combo_win_e": "Win + E 文件资源管理器", "combo_win_l": "Win + L 锁屏",
        "combo_alt_f4": "Alt + F4 关闭窗口", "combo_task_manager": "Ctrl + Shift + Esc 任务管理器", "combo_copy": "Ctrl + C 复制",
        "combo_paste": "Ctrl + V 粘贴", "combo_undo": "Ctrl + Z 撤销", "combo_print_screen": "PrintScreen 截屏",
        "combo_cmd_tab": "⌘ + Tab 切换应用", "combo_spotlight": "⌘ + 空格 聚焦搜索", "combo_mission_control": "⌃ + ↑ 调度中心",
        "combo_cmd_w": "⌘ + W 关闭窗口", "combo_cmd_q": "⌘ + Q 退出应用", "combo_force_quit": "⌘ + ⌥ + Esc 强制退出",
        "combo_lock_mac": "⌃ + ⌘ + Q 锁定屏幕", "combo_screenshot_mac": "⌘ + ⇧ + 5 截屏", "combo_copy_mac": "⌘ + C 复制",
        "combo_paste_mac": "⌘ + V 粘贴", "combo_undo_mac": "⌘ + Z 撤销",
        "action_back": "返回", "action_home": "主页", "action_recents": "最近任务", "action_notifications": "通知栏",
        "action_quickSettings": "快捷设置", "action_screenshot": "截屏", "action_lockScreen": "锁屏", "action_powerMenu": "电源菜单",
        "action_volumeUp": "音量 +", "action_volumeDown": "音量 −",
        // 提示
        "notice_no_displays": "对方没有可采集的显示器",
        "notice_wayland": "对方电脑是 Wayland 桌面：目前只能采集和操作 X11（XWayland）程序。请在对方电脑的登录界面改用 X11（Xorg）会话",
        "notice_capture_failed": "对方的屏幕采集失败，正在重试：%@", "notice_audio_failed": "对方无法采集声音：%@",
        "notice_android_no_input": "对方手机没有开启 Airhand 的“远程操作”（无障碍服务），只能看不能操作",
        "notice_mac_no_screen_input": "对方 Mac 没有允许 Airhand 使用“屏幕与系统录音”和“辅助功能”：看不到画面、听不到声音，也不能操作",
        "notice_mac_no_input": "对方 Mac 没有允许 Airhand 使用“辅助功能”，不能操作键盘和鼠标",
        "notice_mac_no_screen": "对方 Mac 没有允许 Airhand 使用“屏幕与系统录音”，看不到画面，也听不到声音",
        // 问题
        "problem_server_unreachable": "无法连接服务器：%@",
        "problem_cert_mismatch": "服务器证书与设置里的指纹（或第一次连接时记住的证书）不一致，连接可能被人拦截。服务器确实换了证书的话，在设置里重新保存一次证书指纹",
        "problem_incompatible_update_app": "与服务器版本不兼容：服务器较新，请更新 Airhand",
        "problem_incompatible_update_server": "与服务器版本不兼容：服务器较旧，请更新服务器",
        "problem_server_lost": "与服务器的连接已断开：%@", "problem_not_connected": "尚未连接到服务器，请稍候重试",
        "problem_invalid_code": "控制码应为 6 位数字", "problem_code_not_found": "控制码不存在或对方不在线",
        "problem_rate_limited": "尝试过于频繁，请稍后再试", "problem_host_busy": "对方当前连接的设备数已满",
        "problem_server_timeout": "服务器响应超时", "problem_host_offline": "对方已离线",
        "problem_host_key_changed": "对方设备的身份和上次连接时不一样：可能是对方重装了 Airhand，也可能有人在冒充它。确认是对方重装之后，输入对方现在的配对码重新配对",
        "problem_pairing_required": "第一次连接这台设备，需要配对码：电脑在对方的 Airhand 托盘菜单里，手机在对方 Airhand 的“共享本机”页",
        "problem_pairing_failed": "配对码不对，请核对对方显示的配对码",
        "problem_pairing_unavailable": "对方暂停了配对（有人多次输错配对码）：请对方换一个配对码",
        "problem_peer_incompatible_update_app": "与对方设备的版本不兼容：对方较新，请更新这台设备上的 Airhand",
        "problem_peer_incompatible_update_peer": "与对方设备的版本不兼容：对方较旧，请对方更新 Airhand",
        "problem_host_ended": "对方结束了会话", "problem_host_unresponsive": "对方无响应", "problem_kicked": "对方断开了你的连接",
        "problem_code_changed": "对方更换了控制码", "problem_session_ended": "会话已结束", "problem_no_session": "会话不存在",
        "problem_failed": "出错了：%@",
    ]

    // MARK: English
    static let enTable: [String: String] = [
        "cancel": "Cancel", "close": "Close", "save": "Save", "delete": "Delete", "done": "Done", "undo": "Undo",
        "copy": "Copy", "copied": "Copied", "share": "Share", "ok": "OK", "on": "On", "off": "Off", "retry": "Retry now",
        "connect": "Connect", "connecting": "Connecting…", "disconnect": "Disconnect", "rename": "Rename", "forget": "Forget",
        "unknown_device": "Unknown device", "hardware": "hardware", "software": "software", "open_settings": "Open Settings",
        "tab_connect": "Remote", "tab_share": "Share", "tab_settings": "Settings",
        "server_online": "Connected to the server", "server_connecting": "Connecting to the server…", "server_offline": "Not connected",
        "server_not_set": "No server configured",
        "connect_title": "Connect to a device", "connect_hint": "Enter the 6-digit control code shown in Airhand on the other device",
        "connect_paste": "Paste", "connect_no_code_in_clipboard": "No control code in the clipboard",
        "my_devices": "My devices", "my_devices_empty": "Devices you connect to are saved here — one tap to connect next time",
        "device_waiting": "Offline · connects when it's back", "copy_code": "Copy control code",
        "device_delete_title": "Delete device?", "device_delete_text": "Remove “%@” from My devices. You'll need the control code to connect again.",
        "device_deleted": "Deleted “%@”", "device_name_placeholder": "Name",
        "last_connected": "Last connected %@", "session_active": "Connecting · tap to return", "session_live_card": "Remote session · tap to return",
        "setup_server_title": "Set up a server first", "setup_server_body": "Airhand connects devices through your own relay server. Enter its address in Settings to get started.",
        "go_settings": "Open Settings",
        "pairing_title": "Enter the pairing code", "pairing_hint": "The 6-digit pairing code shown on the other device (in the Airhand tray menu on a computer, on the Share page on a phone). It remembers this device afterwards.",
        "pairing_label": "Pairing code", "session_safety": "Safety code %@",
        "share_intro_title": "Let someone view this device", "share_intro_body": "Once you start sharing, others can see this device's screen and hear its sound by entering the control code and pairing code (needed only the first time) in Airhand. Only share them with people you trust.",
        "share_view_only": "iOS doesn't let other apps operate this device, so viewers can watch but not control.",
        "share_start": "Start sharing the screen", "share_start_hint": "Then tap “Start Broadcast” in the system sheet",
        "share_your_code": "Control code", "share_ready": "Waiting for someone to connect", "share_pairing": "Pairing code",
        "share_pairing_desc": "Devices connecting for the first time also need this pairing code; after that the control code is enough",
        "share_pairing_locked": "Pairing is paused after too many wrong codes. Get a new pairing code to resume", "share_pairing_new": "New pairing code",
        "share_forget_paired": "Forget %d paired devices", "share_forget_paired_title": "Forget paired devices?",
        "share_forget_paired_text": "They'll need the pairing code next time. Devices connected now stay connected.",
        "share_viewers": "Viewing this device", "share_no_viewers": "Nobody is connected yet", "share_kick_all": "Disconnect all",
        "share_kick_text": "Disconnect everyone viewing this device? Paired devices can reconnect later; forget them or change the control code to stop that.",
        "share_new_code": "Change control code", "share_new_code_title": "Change the control code?",
        "share_new_code_text": "The old code stops working immediately, connected devices are disconnected, and devices that saved this one can no longer connect. The pairing code changes too and paired devices are forgotten.",
        "share_stop": "Stop sharing", "share_stop_title": "Stop sharing?", "share_stop_text": "Devices viewing this one will be disconnected.",
        "share_code_message": "Connect to my device with Airhand — control code: %@, pairing code (first time only): %@",
        "share_connecting": "Connecting to the server…", "share_sharing": "Sharing the screen", "share_no_group": "This build has no App Group entitlement, so the broadcast extension can't read the settings. Use a signed build.",
        "share_audio_note": "Viewers hear what this device is playing (the microphone is never recorded).",
        "share_stopped_by_user": "Sharing stopped", "share_ext_title": "%@ connected to this device", "share_ext_paired_title": "%@ just paired and connected",
        "share_ext_text": "They can see this device's screen. From %@. Safety code %@: if nobody is in between, they see the same code.",
        "share_ext_failed": "Can't start sharing: %@", "share_ext_no_server": "No server configured. Set one up in Airhand's settings first",
        "share_paired_badge": "New",
        "settings_server": "Server", "server_address": "Server address", "server_address_hint": "host:port, e.g. relay.example.com:4433",
        "server_address_invalid": "Use the form host:port", "server_fingerprint": "Certificate fingerprint", "fingerprint_none": "Not set: the certificate is remembered on first connection",
        "fingerprint_learned": "Not set; remembered the certificate from the first connection (%@…)",
        "fingerprint_hint": "The SHA-256 fingerprint printed in the server log (64 hex digits, colons allowed). Leave empty to remember the certificate on first connection",
        "fingerprint_invalid": "The fingerprint should be 64 hexadecimal digits", "settings_this_device": "This device", "device_name": "Device name",
        "device_name_hint": "The name others see when you connect", "settings_current_code": "Currently %@ · the old code stops working when changed",
        "settings_no_code": "No control code yet (assigned when you first share)", "settings_code_reset": "A new control code will be assigned next time you share",
        "settings_paired": "Paired devices", "settings_paired_none": "None yet (first-time devices need the pairing code)",
        "settings_paired_count": "%d devices can connect with just the control code", "settings_remote": "Remote control", "default_quality": "Default quality",
        "touch_mode": "Input mode", "mode_auto": "Automatic", "mode_auto_desc": "Trackpad for computers, touch for phones",
        "mode_trackpad": "Trackpad", "mode_trackpad_desc": "Slide to move the pointer, tap to click — precise control of a computer",
        "mode_touch": "Touch", "mode_touch_desc": "Tap where you want to click, like your own phone",
        "play_audio": "Play the remote sound", "play_audio_desc": "Turn on sound after connecting",
        "av_sync": "Audio/video sync", "av_sync_desc": "Aligns the picture with the sound for videos; adds input latency. Applies while sound is playing",
        "nav_keys": "Navigation keys for phones", "nav_keys_desc": "Show Back, Home and Recents below the picture",
        "local_cursor": "Local cursor while dragging windows", "local_cursor_desc": "Keep drawing the local pointer when the remote draws its cursor into the picture",
        "settings_data": "Data", "clear_devices": "Clear saved devices", "clear_devices_desc": "%d devices",
        "clear_devices_confirm": "All devices in My devices will be removed.",
        "data_note": "All data (settings, saved devices, this device's control code) stays in Airhand's own folder on this device — never backed up, deleted with the app.",
        "settings_about": "About", "version": "Version", "version_value": "%@ · protocol %d", "codec": "Codec",
        "codec_auto": "Automatic", "codec_auto_value": "Automatic (%@)", "show_tips": "Gestures",
        "server_restart_note": "Saving reconnects with the new settings",
        "preset_saver": "Data saver", "preset_saver_desc": "720p · 24 fps · 1.2 Mbps, for mobile data",
        "preset_balanced": "Balanced", "preset_balanced_desc": "1080p · 30 fps · 4 Mbps",
        "preset_smooth": "Smooth", "preset_smooth_desc": "1080p · 60 fps · 8 Mbps",
        "preset_hd": "HD", "preset_hd_desc": "1440p · 60 fps · 16 Mbps",
        "preset_native": "Native", "preset_native_desc": "Native resolution · 60 fps · 30 Mbps",
        "encoder_info": "%@ %d×%d (%@, %@)",
        "session_connecting": "Connecting to %@…", "session_waiting_video": "Waiting for the picture…", "session_reconnecting": "Connection lost, reconnecting…",
        "session_waiting_host": "%@ is offline", "session_waiting_host_detail": "Connects automatically when it's back online", "session_ended": "Session ended",
        "disconnect_title": "Disconnect?", "disconnect_text": "Disconnect from “%@”.",
        "latency_ms": "%d ms", "path_direct": "Direct", "path_relayed": "Relayed",
        "clipboard_received": "Copied the remote clipboard", "audio_playback_failed": "Can't play sound: %@",
        "menu_keyboard": "Keyboard", "menu_shortcuts": "Shortcuts", "menu_sound": "Sound", "menu_mic": "Microphone",
        "menu_mouse_lock": "Lock mouse", "menu_quality": "Quality", "menu_clipboard": "Clipboard", "menu_displays": "Displays",
        "menu_fit": "Fit screen", "menu_rotate": "Rotate", "menu_help": "Gestures", "menu_files": "Send files",
        "menu_game": "Game controls", "menu_minimize": "Home", "menu_mode": "Input mode", "menu_more": "More",
        "hide_keyboard": "Hide keyboard", "display_n": "Display %d", "display_primary": "Main display",
        "clipboard_send": "Send to the remote clipboard", "clipboard_send_desc": "Send the text you copied here so it can be pasted there",
        "clipboard_sent": "Sent to the remote clipboard", "clipboard_type": "Type it", "clipboard_type_desc": "Type the copied text into the remote's focused field",
        "clipboard_empty": "No text in the clipboard",
        "mic_unavailable": "The remote can't take a microphone right now", "mic_busy": "Another device is using the remote microphone", "mic_denied": "Microphone access denied. Allow it in Settings",
        "mic_on_toast": "Microphone on — they can hear you",
        "privacy_toggle": "Privacy mode",
        "privacy_off": "Blanks all of the remote computer's monitors and disables its keyboard and mouse, so only you can see and control it (the picture moves to a virtual display). Turns off by itself after you disconnect",
        "privacy_off_mac": "Blanks all of the remote Mac's displays and disables its keyboard, mouse and trackpad, so only you can see and control it. Turns off by itself after you disconnect",
        "privacy_starting": "Turning on: the remote computer is switching to a virtual display, this takes a few seconds", "privacy_starting_mac": "Turning on",
        "privacy_on": "On: the remote monitors are blank and its keyboard and mouse don't work. Turns off after you disconnect; if the connection drops it waits 30 seconds for you to reconnect and stays on if you do",
        "privacy_failed": "Couldn't turn on privacy mode (%@), the remote screen is back to normal. You can try again",
        "privacy_unavailable": "Not available on the remote computer: Airhand has to be installed for all users, together with the virtual display driver",
        "privacy_unavailable_mac": "Not available on the remote Mac: someone at that Mac has to allow Airhand in System Settings → Privacy & Security → Accessibility first (it's needed to block its keyboard and mouse)",
        "privacy_not_allowed": "The remote computer's settings don't allow privacy mode (privacy_mode in config.toml)",
        "privacy_note": "Someone at the remote computer can end privacy mode by disconnecting it from the network or restarting it",
        "view_only_host": "The remote is an iPhone / iPad: view only",
        "files_sent": "Put on the remote clipboard — paste it there", "files_received": "The remote copied %d items",
        "files_save": "Save to Files", "files_saved": "Saved to Files › On My iPhone › Airhand", "files_failed": "Couldn't save %@",
        "files_saving": "Receiving…",
        "pointer_locked": "Mouse locked · hold Esc or use the menu to release",
        "tips_trackpad": "Trackpad mode", "tips_touch": "Touch mode", "tips_phone": "Controlling a phone",
        "tips_menu": "The round button on the edge is the menu: keyboard, shortcuts, input mode, quality… Drag it anywhere.",
        "g_slide": "One-finger slide", "g_tap": "Tap", "g_long_press": "Long press", "g_tap_then_drag": "Tap, then hold and drag", "g_two_tap": "Two-finger tap",
        "g_two_slide": "Two-finger drag", "g_pinch": "Pinch", "g_three_tap": "Three-finger tap", "g_move_pointer": "Move the pointer",
        "g_left_click": "Left click (tap twice = double-click)", "g_right_click": "Right click", "g_click_there": "Click there",
        "g_right_click_there": "Right-click there", "g_drag": "Drag with the left button (select text, move windows)", "g_scroll": "Scroll (with inertia)",
        "g_zoom": "Zoom (the picture follows the pointer)", "g_zoom_pan": "Zoom / move the picture", "g_keyboard": "Show / hide the keyboard",
        "g_same_as_phone": "Goes straight to the remote phone", "g_mouse": "Mouse and keyboard", "g_mouse_desc": "Map to the remote mouse and keyboard; on iPad the mouse can be locked for games",
        "game_show": "Show on the picture", "game_show_desc": "Hold a button to hold its key; elsewhere works as usual, and dragging turns the view when a game hides the cursor",
        "game_layouts": "Layouts", "game_no_layouts": "No layouts yet — create one", "game_new": "New layout", "game_edit": "Edit layout",
        "game_new_title": "Start from a template", "game_duplicate": "Duplicate", "game_layout_name": "Layout name",
        "game_delete_title": "Delete “%@”?", "game_copy_name": "%@ copy",
        "game_tpl_shooter": "Shooter (WASD)", "game_tpl_shooter_desc": "Stick (push to the edge to sprint), fire (hold and drag to aim), aim, jump, crouch, reload, weapons 1–3",
        "game_tpl_skills": "Skills (MOBA / ARPG)", "game_tpl_skills_desc": "Q W E R, D F, items 1–4, B, Space; tap the picture to click",
        "game_tpl_arrows": "Arrow keys (platformers / emulators)", "game_tpl_arrows_desc": "Arrow-key stick, Z X C, Space, Enter, Esc",
        "game_tpl_blank": "Blank", "game_tpl_blank_desc": "Add your own buttons",
        "game_add": "Add", "game_layout_settings": "Layout settings", "game_edit_hint": "Drag to place · pinch to resize · tap to edit",
        "game_discard_title": "Discard changes?", "game_discard_text": "Changes to this layout won't be saved.", "game_discard": "Discard",
        "game_key": "Button", "game_stick": "Stick", "game_sends": "Sends", "game_change": "Change", "game_label": "Label",
        "game_label_hint": "Leave empty to show the key name", "game_mode": "Behavior", "game_mode_hold": "Hold", "game_mode_toggle": "Tap to toggle",
        "game_look": "Drag to turn the view", "game_look_desc": "While held, e.g. aim while firing", "game_size": "Size",
        "game_stick_keys": "Directions", "game_sprint": "Extra key at the edge", "game_sprint_none": "None",
        "game_floating": "Start where you touch", "game_floating_desc": "No need to hit the center of the stick",
        "game_opacity": "Opacity", "game_look_speed": "Look sensitivity", "game_haptics": "Vibrate on press",
        "game_pick_title": "Choose keys", "game_pick_combo": "Combination (up to 3 keys, pressed together in order)",
        "game_group_mouse": "Mouse", "game_group_common": "Common", "game_group_letters": "Letters", "game_group_digits": "Digits",
        "game_group_function": "Function keys", "game_group_nav": "Navigation & editing", "game_group_symbols": "Symbols",
        "game_mouse_left": "Left", "game_mouse_right": "Right", "game_mouse_middle": "Middle", "game_wheel_up": "Wheel ↑", "game_wheel_down": "Wheel ↓",
        "game_duplicate_item": "Duplicate", "game_empty_layout_hint": "No buttons yet — tap + above to add one", "game_tip": "Hold a button to hold its key; drag on empty space to turn the view when a game hides the cursor. Menu → Game controls → Edit layout to arrange them.",
        "combo_ctrl_alt_del": "Ctrl + Alt + Del", "combo_alt_tab": "Alt + Tab switch windows", "combo_win": "Win Start menu",
        "combo_win_d": "Win + D show desktop", "combo_win_e": "Win + E File Explorer", "combo_win_l": "Win + L lock",
        "combo_alt_f4": "Alt + F4 close window", "combo_task_manager": "Ctrl + Shift + Esc Task Manager", "combo_copy": "Ctrl + C copy",
        "combo_paste": "Ctrl + V paste", "combo_undo": "Ctrl + Z undo", "combo_print_screen": "PrintScreen screenshot",
        "combo_cmd_tab": "⌘ + Tab switch apps", "combo_spotlight": "⌘ + Space Spotlight", "combo_mission_control": "⌃ + ↑ Mission Control",
        "combo_cmd_w": "⌘ + W close window", "combo_cmd_q": "⌘ + Q quit app", "combo_force_quit": "⌘ + ⌥ + Esc force quit",
        "combo_lock_mac": "⌃ + ⌘ + Q lock screen", "combo_screenshot_mac": "⌘ + ⇧ + 5 screenshot", "combo_copy_mac": "⌘ + C copy",
        "combo_paste_mac": "⌘ + V paste", "combo_undo_mac": "⌘ + Z undo",
        "action_back": "Back", "action_home": "Home", "action_recents": "Recents", "action_notifications": "Notifications",
        "action_quickSettings": "Quick settings", "action_screenshot": "Screenshot", "action_lockScreen": "Lock screen", "action_powerMenu": "Power menu",
        "action_volumeUp": "Volume +", "action_volumeDown": "Volume −",
        "notice_no_displays": "The remote has no display to capture",
        "notice_wayland": "The remote computer runs a Wayland desktop: only X11 (XWayland) apps can be captured and controlled. Switch to an X11 (Xorg) session at its login screen",
        "notice_capture_failed": "Screen capture failed on the remote, retrying: %@", "notice_audio_failed": "The remote can't capture sound: %@",
        "notice_android_no_input": "Remote control (the accessibility service) isn't enabled on the remote phone: view only",
        "notice_mac_no_screen_input": "The remote Mac hasn't allowed Airhand “Screen & System Audio Recording” and “Accessibility”: no picture, no sound, no control",
        "notice_mac_no_input": "The remote Mac hasn't allowed Airhand “Accessibility”: keyboard and mouse don't work",
        "notice_mac_no_screen": "The remote Mac hasn't allowed Airhand “Screen & System Audio Recording”: no picture or sound",
        "problem_server_unreachable": "Can't reach the server: %@",
        "problem_cert_mismatch": "The server certificate doesn't match the fingerprint in Settings (or the one remembered on first connection); the connection may be intercepted. If the server really changed its certificate, save the fingerprint again in Settings",
        "problem_incompatible_update_app": "Incompatible with the server: the server is newer, please update Airhand",
        "problem_incompatible_update_server": "Incompatible with the server: the server is older, please update it",
        "problem_server_lost": "Lost the connection to the server: %@", "problem_not_connected": "Not connected to the server yet, try again shortly",
        "problem_invalid_code": "The control code is 6 digits", "problem_code_not_found": "Unknown control code, or the device is offline",
        "problem_rate_limited": "Too many attempts, try again later", "problem_host_busy": "The remote has too many connected devices",
        "problem_server_timeout": "The server didn't respond", "problem_host_offline": "The remote is offline",
        "problem_host_key_changed": "The remote's identity changed since last time: Airhand may have been reinstalled there, or someone is impersonating it. Once you're sure it was reinstalled, pair again with its current pairing code",
        "problem_pairing_required": "First connection to this device: enter its pairing code (in the Airhand tray menu on a computer, on the Share page on a phone)",
        "problem_pairing_failed": "Wrong pairing code — check the code shown on the other device",
        "problem_pairing_unavailable": "Pairing is paused on the remote (too many wrong codes): ask them to get a new pairing code",
        "problem_peer_incompatible_update_app": "Incompatible with the remote: it's newer, please update Airhand here",
        "problem_peer_incompatible_update_peer": "Incompatible with the remote: it's older, ask them to update Airhand",
        "problem_host_ended": "The remote ended the session", "problem_host_unresponsive": "The remote isn't responding", "problem_kicked": "The remote disconnected you",
        "problem_code_changed": "The remote changed its control code", "problem_session_ended": "The session ended", "problem_no_session": "No such session",
        "problem_failed": "Something went wrong: %@",
    ]
}

/// 问题、提示的文字（与 Android 的 Messages.kt 相同的规则）
enum Messages {
    static func problem(_ p: Problem) -> String {
        let d = p.detail ?? ""
        switch p.kind {
        case "serverUnreachable": return L.t("problem_server_unreachable", d)
        case "certMismatch": return L.t("problem_cert_mismatch")
        case "incompatible":
            return (p.server ?? 0) > (p.client ?? 0) ? L.t("problem_incompatible_update_app") : L.t("problem_incompatible_update_server")
        case "serverLost": return L.t("problem_server_lost", d)
        case "notConnected": return L.t("problem_not_connected")
        case "invalidCode": return L.t("problem_invalid_code")
        case "codeNotFound": return L.t("problem_code_not_found")
        case "rateLimited": return L.t("problem_rate_limited")
        case "hostBusy": return L.t("problem_host_busy")
        case "serverTimeout": return L.t("problem_server_timeout")
        case "hostOffline": return L.t("problem_host_offline")
        case "hostKeyChanged": return L.t("problem_host_key_changed")
        case "pairingRequired": return L.t("problem_pairing_required")
        case "pairingFailed": return L.t("problem_pairing_failed")
        case "pairingUnavailable": return L.t("problem_pairing_unavailable")
        case "peerIncompatible":
            return (p.peer ?? 0) > (p.ours ?? 0) ? L.t("problem_peer_incompatible_update_app") : L.t("problem_peer_incompatible_update_peer")
        case "hostEnded": return L.t("problem_host_ended")
        case "hostUnresponsive": return L.t("problem_host_unresponsive")
        case "kicked": return L.t("problem_kicked")
        case "codeChanged": return L.t("problem_code_changed")
        case "sessionEnded": return L.t("problem_session_ended")
        case "noSession": return L.t("problem_no_session")
        default: return L.t("problem_failed", d.isEmpty ? p.kind : d)
        }
    }

    static func notice(_ n: Notice, host: OsKind) -> String? {
        switch n.kind {
        case "noDisplays": return L.t("notice_no_displays")
        case "wayland": return L.t("notice_wayland")
        case "captureFailed": return L.t("notice_capture_failed", n.detail ?? "")
        case "audioFailed": return L.t("notice_audio_failed", n.detail ?? "")
        case "missingPermissions":
            if host == .android { return n.input ? L.t("notice_android_no_input") : nil }
            if n.screen && n.input { return L.t("notice_mac_no_screen_input") }
            if n.input { return L.t("notice_mac_no_input") }
            if n.screen { return L.t("notice_mac_no_screen") }
            return nil
        default: return nil
        }
    }
}

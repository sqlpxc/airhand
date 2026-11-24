import SwiftUI
import UIKit

struct SessionMenuActions {
    var keyboard: () -> Void
    var fit: () -> Void
    var rotate: (() -> Void)?
    var files: () -> Void
    var mode: (String) -> Void
    var editGame: (GameLayout) -> Void
    var tips: () -> Void
    var disconnect: () -> Void
    var minimize: () -> Void
}

/// 远程画面的菜单（从画面边上的圆钮打开）：顶部是对方和连接信息，下面是常用操作和几个子页面
struct SessionMenu: View {
    let app: AppModel
    @Bindable var session: RemoteSession
    let ui: SessionUIState
    let actions: SessionMenuActions
    @Environment(\.dismiss) private var dismiss

    private var computer: Bool { !session.os.isPhone }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    header
                    tiles
                    pages
                }
                .padding(.horizontal, 18)
                .padding(.vertical, 12)
            }
            .background(Theme.bg.ignoresSafeArea())
            .toolbar(.hidden, for: .navigationBar)
        }
        .preferredColorScheme(.dark)
    }

    // ── 顶部 ──

    private var header: some View {
        HStack(spacing: 14) {
            DeviceIcon(os: session.os, size: 48)
            VStack(alignment: .leading, spacing: 4) {
                Text(session.hostName).font(.title3.weight(.bold)).foregroundStyle(Theme.text).lineLimit(1)
                Text(statsLine).font(.caption).foregroundStyle(Theme.sub).lineLimit(2)
                if let s = session.safety {
                    Label(L.t("session_safety", s), systemImage: "checkmark.shield").font(.caption2).foregroundStyle(Theme.faint)
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.top, 8)
    }

    private var statsLine: String {
        var parts: [String] = []
        if let e = session.encoder { parts.append(e.codec.short) }
        if let s = session.stats {
            parts.append("\(Int(s.fps.rounded())) fps")
            if let r = s.rttMs { parts.append(L.t("latency_ms", Int(r.rounded()))) }
            parts.append(s.kbps >= 1000 ? String(format: "%.1f Mbps", s.kbps / 1000) : "\(Int(s.kbps)) kbps")
            if s.width > 0 { parts.append("\(s.width)×\(s.height)") }
            parts.append(s.direct ? L.t("path_direct") : L.t("path_relayed"))
        }
        return parts.isEmpty ? (session.hostOs.isEmpty ? Code.format(session.code) : session.hostOs) : parts.joined(separator: " · ")
    }

    // ── 常用操作 ──

    private var tiles: some View {
        LazyVGrid(columns: [GridItem(.adaptive(minimum: 84), spacing: 10)], spacing: 10) {
            if !session.viewOnly {
                tile("keyboard", L.t("menu_keyboard")) { actions.keyboard() }
            }
            tile(session.audioOn ? "speaker.wave.2.fill" : "speaker.slash", L.t("menu_sound"), on: session.audioOn) {
                session.setAudio(!session.audioOn)
            }
            if let st = session.micState, !session.viewOnly {
                tile(session.micOn ? "mic.fill" : "mic.slash", L.t("menu_mic"), on: session.micOn, dim: st != "ready" && !session.micOn) {
                    session.setMic(!session.micOn)
                }
            }
            if computer {
                tile(ui.effectiveMode == "touch" ? "hand.point.up.left" : "rectangle.and.hand.point.up.left",
                     ui.effectiveMode == "touch" ? L.t("mode_touch") : L.t("mode_trackpad")) {
                    actions.mode(ui.effectiveMode == "touch" ? "trackpad" : "touch")
                }
                tile(session.mouseLock ? "lock.fill" : "lock.open", L.t("menu_mouse_lock"), on: session.mouseLock) {
                    session.mouseLock.toggle()
                }
            }
            tile("arrow.down.right.and.arrow.up.left", L.t("menu_fit")) { actions.fit() }
            if let r = actions.rotate { tile("rotate.right", L.t("menu_rotate"), action: r) }
            if !session.viewOnly {
                tile("doc.badge.arrow.up", L.t("menu_files")) { actions.files() }
            }
            tile("hand.raised", L.t("menu_help")) { actions.tips() }
            tile("rectangle.portrait.and.arrow.forward", L.t("menu_minimize")) { actions.minimize() }
            tile("xmark.circle.fill", L.t("disconnect"), tint: Theme.bad) { actions.disconnect() }
        }
    }

    private func tile(_ icon: String, _ title: String, on: Bool = false, dim: Bool = false, tint: Color = Theme.text,
                      action: @escaping () -> Void) -> some View {
        Button {
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
            action()
        } label: {
            VStack(spacing: 8) {
                Image(systemName: icon).font(.system(size: 20, weight: .semibold))
                    .foregroundStyle(on ? .white : tint)
                Text(title).font(.caption.weight(.medium)).foregroundStyle(on ? .white : Theme.sub).lineLimit(1).minimumScaleFactor(0.7)
            }
            .frame(maxWidth: .infinity, minHeight: 74)
            .background {
                RoundedRectangle(cornerRadius: 16, style: .continuous)
                    .fill(on ? AnyShapeStyle(Theme.gradient) : AnyShapeStyle(Theme.card))
            }
            .overlay(RoundedRectangle(cornerRadius: 16, style: .continuous).stroke(Theme.stroke))
            .opacity(dim ? 0.5 : 1)
        }
        .buttonStyle(.plain)
    }

    // ── 子页面 ──

    private var pages: some View {
        VStack(spacing: 0) {
            if !session.viewOnly {
                NavigationLink { ShortcutsPage(session: session) } label: { row("command", L.t("menu_shortcuts")) }
                Divider().overlay(Theme.stroke).padding(.leading, 50)
            }
            NavigationLink { QualityPage(app: app, session: session) } label: {
                row("sparkles.tv", L.t("menu_quality"), value: Preset.match(session.quality).map { L.t("preset_\($0.rawValue)") })
            }
            if !session.viewOnly {
                Divider().overlay(Theme.stroke).padding(.leading, 50)
                NavigationLink { ClipboardPage(app: app, session: session) } label: { row("doc.on.clipboard", L.t("menu_clipboard")) }
            }
            if session.displays.count > 1 {
                Divider().overlay(Theme.stroke).padding(.leading, 50)
                NavigationLink { DisplaysPage(session: session) } label: { row("display.2", L.t("menu_displays")) }
            }
            // 防窥模式：对方（Windows、Mac）说了它的状态才有；点开是开关和说明（开不了时写着为什么）
            if let p = session.privacyState, !session.viewOnly {
                Divider().overlay(Theme.stroke).padding(.leading, 50)
                NavigationLink { PrivacyPage(session: session) } label: {
                    row(p.active ? "lock.shield.fill" : "lock.shield", L.t("privacy_toggle"), value: L.t(p.active ? "on" : "off"))
                }
            }
            if computer && !session.viewOnly {
                Divider().overlay(Theme.stroke).padding(.leading, 50)
                NavigationLink { GamePage(app: app, session: session, edit: actions.editGame) } label: {
                    row("gamecontroller", L.t("menu_game"), value: app.games.pref(session.code).enabled ? L.t("on") : L.t("off"))
                }
            }
        }
        .background(Theme.card, in: RoundedRectangle(cornerRadius: 18, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 18, style: .continuous).stroke(Theme.stroke))
    }

    private func row(_ icon: String, _ title: String, value: String? = nil) -> some View {
        HStack(spacing: 14) {
            Image(systemName: icon).font(.system(size: 17, weight: .semibold)).foregroundStyle(Theme.c3).frame(width: 24)
            Text(title).foregroundStyle(Theme.text)
            Spacer()
            if let value { Text(value).foregroundStyle(Theme.sub).font(.subheadline) }
            Image(systemName: "chevron.right").font(.footnote.weight(.semibold)).foregroundStyle(Theme.faint)
        }
        .padding(.horizontal, 16)
        .frame(minHeight: 52)
        .contentShape(Rectangle())
    }
}

// ───────────────────────────── 快捷键 ─────────────────────────────

struct ShortcutsPage: View {
    let session: RemoteSession
    @Environment(\.dismiss) private var dismiss

    private var combos: [(String, [String])] {
        switch session.os {
        case .mac:
            return [("combo_cmd_tab", ["MetaLeft", "Tab"]), ("combo_spotlight", ["MetaLeft", "Space"]),
                    ("combo_mission_control", ["ControlLeft", "ArrowUp"]), ("combo_cmd_w", ["MetaLeft", "KeyW"]),
                    ("combo_cmd_q", ["MetaLeft", "KeyQ"]), ("combo_force_quit", ["MetaLeft", "AltLeft", "Escape"]),
                    ("combo_lock_mac", ["ControlLeft", "MetaLeft", "KeyQ"]), ("combo_screenshot_mac", ["MetaLeft", "ShiftLeft", "Digit5"]),
                    ("combo_copy_mac", ["MetaLeft", "KeyC"]), ("combo_paste_mac", ["MetaLeft", "KeyV"]), ("combo_undo_mac", ["MetaLeft", "KeyZ"])]
        case .android, .ios:
            return []
        default:
            return [("combo_ctrl_alt_del", ["ControlLeft", "AltLeft", "Delete"]), ("combo_alt_tab", ["AltLeft", "Tab"]),
                    ("combo_win", ["MetaLeft"]), ("combo_win_d", ["MetaLeft", "KeyD"]), ("combo_win_e", ["MetaLeft", "KeyE"]),
                    ("combo_win_l", ["MetaLeft", "KeyL"]), ("combo_alt_f4", ["AltLeft", "F4"]),
                    ("combo_task_manager", ["ControlLeft", "ShiftLeft", "Escape"]), ("combo_copy", ["ControlLeft", "KeyC"]),
                    ("combo_paste", ["ControlLeft", "KeyV"]), ("combo_undo", ["ControlLeft", "KeyZ"]), ("combo_print_screen", ["PrintScreen"])]
        }
    }

    var body: some View {
        List {
            if let actions = session.hello?.actions, !actions.isEmpty {
                Section {
                    ForEach(actions, id: \.self) { a in
                        Button(L.t("action_\(a.rawValue)")) {
                            session.system(a)
                            dismiss()
                        }
                    }
                }
            }
            if !combos.isEmpty {
                Section {
                    ForEach(combos, id: \.0) { key, codes in
                        Button(L.t(key)) {
                            UIImpactFeedbackGenerator(style: .light).impactOccurred()
                            session.input(Input.chord(codes))
                        }
                    }
                }
            }
        }
        .scrollContentBackground(.hidden)
        .background(Theme.bg)
        .navigationTitle(L.t("menu_shortcuts"))
        .navigationBarTitleDisplayMode(.inline)
        .toolbar(.visible, for: .navigationBar)
    }
}

// ───────────────────────────── 画质 ─────────────────────────────

struct QualityPage: View {
    let app: AppModel
    let session: RemoteSession

    var body: some View {
        List {
            Section {
                ForEach(Preset.allCases) { p in
                    Button {
                        session.setQuality(p.quality(session.quality.codec))
                        app.settings.preset = p.rawValue
                    } label: {
                        HStack {
                            VStack(alignment: .leading, spacing: 2) {
                                Text(L.t("preset_\(p.rawValue)")).foregroundStyle(Theme.text)
                                Text(L.t("preset_\(p.rawValue)_desc")).font(.caption).foregroundStyle(Theme.sub)
                            }
                            Spacer()
                            if Preset.match(session.quality) == p { Image(systemName: "checkmark").foregroundStyle(Theme.c3) }
                        }
                    }
                }
            }
            Section(L.t("codec")) {
                let usable = session.usableCodecs
                let auto = app.settings.codec.isEmpty || !usable.contains(where: { $0.wire == app.settings.codec })
                Button {
                    session.setCodec(nil)
                } label: {
                    HStack {
                        Text(L.t("codec_auto_value", usable.first?.short ?? "H.264")).foregroundStyle(Theme.text)
                        Spacer()
                        if auto { Image(systemName: "checkmark").foregroundStyle(Theme.c3) }
                    }
                }
                ForEach(usable) { c in
                    Button {
                        session.setCodec(c)
                    } label: {
                        HStack {
                            Text(c.label).foregroundStyle(Theme.text)
                            Spacer()
                            if !auto && session.quality.codec == c { Image(systemName: "checkmark").foregroundStyle(Theme.c3) }
                        }
                    }
                }
                if let e = session.encoder {
                    Text(L.t("encoder_info", e.codec.label, e.width, e.height, e.name, e.hardware ? L.t("hardware") : L.t("software")))
                        .font(.caption).foregroundStyle(Theme.sub)
                }
            }
            Section {
                Toggle(isOn: Binding(get: { session.avSyncOn }, set: { session.setAvSync($0) })) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(L.t("av_sync"))
                        Text(L.t("av_sync_desc")).font(.caption).foregroundStyle(Theme.sub)
                    }
                }
            }
        }
        .scrollContentBackground(.hidden)
        .background(Theme.bg)
        .navigationTitle(L.t("menu_quality"))
        .navigationBarTitleDisplayMode(.inline)
        .toolbar(.visible, for: .navigationBar)
    }
}

// ───────────────────────────── 剪贴板、显示器 ─────────────────────────────

struct ClipboardPage: View {
    let app: AppModel
    let session: RemoteSession
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        List {
            Section {
                Button { send(type: false) } label: { item("doc.on.clipboard", "clipboard_send", "clipboard_send_desc") }
                Button { send(type: true) } label: { item("keyboard.badge.ellipsis", "clipboard_type", "clipboard_type_desc") }
            }
        }
        .scrollContentBackground(.hidden)
        .background(Theme.bg)
        .navigationTitle(L.t("menu_clipboard"))
        .navigationBarTitleDisplayMode(.inline)
        .toolbar(.visible, for: .navigationBar)
    }

    private func item(_ icon: String, _ t: String, _ d: String) -> some View {
        HStack(spacing: 14) {
            Image(systemName: icon).foregroundStyle(Theme.c3).frame(width: 26)
            VStack(alignment: .leading, spacing: 2) {
                Text(L.t(t)).foregroundStyle(Theme.text)
                Text(L.t(d)).font(.caption).foregroundStyle(Theme.sub)
            }
        }
    }

    private func send(type: Bool) {
        guard let text = UIPasteboard.general.string, !text.isEmpty else {
            app.show(L.t("clipboard_empty"))
            return
        }
        if type {
            session.typeText(text)
        } else {
            session.sendClipboard(text)
            app.show(L.t("clipboard_sent"))
        }
        dismiss()
    }
}

struct DisplaysPage: View {
    let session: RemoteSession

    var body: some View {
        List {
            ForEach(session.displays) { d in
                Button { session.selectDisplay(d.index) } label: {
                    HStack {
                        Image(systemName: "display").foregroundStyle(Theme.c3)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(d.name.isEmpty ? L.t("display_n", d.index + 1) : d.name).foregroundStyle(Theme.text)
                            Text("\(d.width)×\(d.height)\(d.primary ? " · " + L.t("display_primary") : "")").font(.caption).foregroundStyle(Theme.sub)
                        }
                        Spacer()
                        if d.index == session.currentDisplay { Image(systemName: "checkmark").foregroundStyle(Theme.c3) }
                    }
                }
            }
        }
        .scrollContentBackground(.hidden)
        .background(Theme.bg)
        .navigationTitle(L.t("menu_displays"))
        .navigationBarTitleDisplayMode(.inline)
        .toolbar(.visible, for: .navigationBar)
    }
}

// ───────────────────────────── 防窥模式 ─────────────────────────────

/// 防窥模式：对方所有的显示器黑屏、键盘鼠标不起作用，只有控制端看得到、操作得了（和桌面版工具栏的盾牌菜单一样）。
/// 开关下面写着现在怎么样：正在打开、开着、没能打开的原因、开不了的原因
struct PrivacyPage: View {
    let session: RemoteSession

    var body: some View {
        List {
            if let s = session.privacyState {
                Section {
                    Toggle(isOn: Binding(get: { session.privacyState?.active ?? false }, set: { session.setPrivacy($0) })) {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(L.t("privacy_toggle"))
                            Text(hint(s)).font(.caption).foregroundStyle(s == .failed ? Theme.bad : Theme.sub)
                        }
                    }
                    .disabled(!s.usable)
                } footer: {
                    if s.usable { Text(L.t("privacy_note")) }
                }
            } else {
                // 会话断了：对方还没重新说（重连上了接着显示，要不要开着已经在握手里告诉它了）
                Text(L.t("session_reconnecting")).foregroundStyle(Theme.sub)
            }
        }
        .scrollContentBackground(.hidden)
        .background(Theme.bg)
        .navigationTitle(L.t("privacy_toggle"))
        .navigationBarTitleDisplayMode(.inline)
        .toolbar(.visible, for: .navigationBar)
    }

    private func hint(_ s: PrivacyState) -> String {
        let mac = session.os == .mac
        switch s {
        case .unavailable: return L.t(mac ? "privacy_unavailable_mac" : "privacy_unavailable")
        case .notAllowed: return L.t("privacy_not_allowed")
        case .starting: return L.t(mac ? "privacy_starting_mac" : "privacy_starting")
        case .on: return L.t("privacy_on")
        case .failed: return L.t("privacy_failed", session.privacyDetail ?? "—")
        case .off: return L.t(mac ? "privacy_off_mac" : "privacy_off")
        }
    }
}

// ───────────────────────────── 游戏按键 ─────────────────────────────

struct GamePage: View {
    let app: AppModel
    let session: RemoteSession
    let edit: (GameLayout) -> Void
    @State private var newSheet = false
    @State private var renaming: GameLayout?
    @State private var name = ""
    @State private var deleting: GameLayout?

    private var store: GameStore { app.games }

    var body: some View {
        List {
            Section {
                Toggle(isOn: Binding(get: { store.pref(session.code).enabled }, set: { store.setEnabled(session.code, $0) })) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(L.t("game_show"))
                        Text(L.t("game_show_desc")).font(.caption).foregroundStyle(Theme.sub)
                    }
                }
            } footer: {
                Text(L.t("game_tip"))
            }
            Section(L.t("game_layouts")) {
                if store.layouts.isEmpty {
                    Text(L.t("game_no_layouts")).foregroundStyle(Theme.sub)
                }
                ForEach(store.layouts) { l in
                    let selected = (store.active(session.code)?.id ?? store.pref(session.code).layout) == l.id
                    Button { store.select(session.code, l.id) } label: {
                        HStack {
                            Image(systemName: selected ? "largecircle.fill.circle" : "circle").foregroundStyle(selected ? Theme.c3 : Theme.faint)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(l.name).foregroundStyle(Theme.text)
                                Text("\(l.items.count)").font(.caption).foregroundStyle(Theme.sub)
                            }
                            Spacer()
                            Menu {
                                Button { edit(l) } label: { Label(L.t("game_edit"), systemImage: "slider.horizontal.3") }
                                Button { name = l.name; renaming = l } label: { Label(L.t("rename"), systemImage: "pencil") }
                                Button { _ = store.duplicate(l) } label: { Label(L.t("game_duplicate"), systemImage: "plus.square.on.square") }
                                Button(role: .destructive) { deleting = l } label: { Label(L.t("delete"), systemImage: "trash") }
                            } label: {
                                Image(systemName: "ellipsis.circle").font(.title3).foregroundStyle(Theme.sub).frame(width: 40, height: 36)
                            }
                        }
                    }
                }
                Button { newSheet = true } label: { Label(L.t("game_new"), systemImage: "plus") }
            }
            if let active = store.active(session.code) {
                Section {
                    Button { edit(active) } label: { Label(L.t("game_edit"), systemImage: "slider.horizontal.3") }
                }
            }
        }
        .scrollContentBackground(.hidden)
        .background(Theme.bg)
        .navigationTitle(L.t("menu_game"))
        .navigationBarTitleDisplayMode(.inline)
        .toolbar(.visible, for: .navigationBar)
        .confirmationDialog(L.t("game_new_title"), isPresented: $newSheet, titleVisibility: .visible) {
            Button(L.t("game_tpl_shooter")) { create(GameTemplates.shooter()) }
            Button(L.t("game_tpl_skills")) { create(GameTemplates.skills()) }
            Button(L.t("game_tpl_arrows")) { create(GameTemplates.arrows()) }
            Button(L.t("game_tpl_blank")) { create(GameTemplates.blank()) }
        }
        .alert(L.t("rename"), isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } })) {
            TextField(L.t("game_layout_name"), text: $name)
            Button(L.t("save")) {
                if var l = renaming, !name.trimmingCharacters(in: .whitespaces).isEmpty {
                    l.name = name.trimmingCharacters(in: .whitespaces)
                    store.update(l)
                }
                renaming = nil
            }
            Button(L.t("cancel"), role: .cancel) { renaming = nil }
        }
        .confirmationDialog(deleting.map { L.t("game_delete_title", $0.name) } ?? "", isPresented: Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } }),
                            titleVisibility: .visible) {
            Button(L.t("delete"), role: .destructive) {
                if let d = deleting { store.delete(d.id) }
                deleting = nil
            }
        }
    }

    private func create(_ l: GameLayout) {
        let added = store.add(l)
        store.select(session.code, added.id)
        if !store.pref(session.code).enabled { store.setEnabled(session.code, true) }
        if l.items.isEmpty { edit(added) }
    }
}

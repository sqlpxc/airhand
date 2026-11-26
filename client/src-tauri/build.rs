fn main() {
    // Media Foundation 延迟加载：Windows N/KN 版（未装媒体功能包）没有 mfplat.dll，
    // 延迟加载后程序仍能启动，并自动回退到软件编码。
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows-msvc") {
        println!("cargo:rustc-link-arg=/DELAYLOAD:mfplat.dll");
        // 链接参数对这个包的所有目标生效：没有用到 Media Foundation 的（airhand.exe 的单元测试程序）链接器会报
        // LNK4199（没有来自 mfplat.dll 的导入，忽略 /DELAYLOAD）。本来就是无事可做，不算问题
        println!("cargo:rustc-link-arg=/IGNORE:4199");
        println!("cargo:rustc-link-lib=delayimp");
    }
    let mic = "src/host/mic";
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => {
            println!("cargo:rerun-if-changed={mic}/pcm.h");
            println!("cargo:rerun-if-changed={mic}/tap.m");
            cc::Build::new()
                .std("gnu11")
                .flag("-fobjc-arc")
                .flag("-fblocks")
                .file(format!("{mic}/tap.m"))
                .compile("airhand_mic");
            // 补屏的虚拟显示器（见 src/host/virtual_display/mac.rs）
            let vd = "src/host/virtual_display/mac.m";
            println!("cargo:rerun-if-changed={vd}");
            cc::Build::new()
                .std("gnu11")
                .flag("-fobjc-arc")
                .flag("-fblocks")
                .file(vd)
                .compile("airhand_vd");
            // 防窥模式盖在本机显示器上的黑窗口（见 src/host/virtual_display/curtain.rs）
            let curtain = "src/host/virtual_display/curtain.m";
            println!("cargo:rerun-if-changed={curtain}");
            cc::Build::new()
                .std("gnu11")
                .flag("-fobjc-arc")
                .flag("-fblocks")
                .file(curtain)
                .compile("airhand_curtain");
            awdl_helper();
            for framework in [
                "AppKit",
                "AVFAudio",
                "CoreAudio",
                "CoreGraphics",
                "Foundation",
            ] {
                println!("cargo:rustc-link-lib=framework={framework}");
            }
        }
        Ok("linux") => {
            println!("cargo:rerun-if-changed={mic}/pcm.h");
            println!("cargo:rerun-if-changed={mic}/pipewire.c");
            let pw = pkg_config::Config::new()
                .cargo_metadata(false)
                .atleast_version("0.3.49")
                .probe("libpipewire-0.3")
                .expect("install libpipewire-0.3-dev to build the virtual microphone");
            cc::Build::new()
                .std("gnu11")
                .includes(pw.include_paths)
                .file(format!("{mic}/pipewire.c"))
                .compile("airhand_mic");
            // The archive that references PipeWire must precede it in the ELF linker's input.
            for path in pw.link_paths {
                println!("cargo:rustc-link-search=native={}", path.display());
            }
            for lib in pw.libs {
                println!("cargo:rustc-link-lib={lib}");
            }
        }
        _ => {}
    }
    tauri_build::build()
}

/// macOS：远程控制时暂停 AWDL 的助手（src/awdl/helper.c）是一个单独的可执行文件，由 launchd 以 root 身份运行。
/// 编译到 OUT_DIR 里，再整个放进 Airhand 的可执行文件（`include_bytes!`）：安装时由管理员权限的脚本写到
/// /Library/PrivilegedHelperTools（见 src/awdl/helper.rs），不经过普通用户写得了的临时文件。
/// 编译器和目标架构（交叉编译 Intel 版时是 x86_64）由 cc 按 `--target` 定；连接器给 Apple 芯片的可执行文件自动签上 ad-hoc 签名
fn awdl_helper() {
    let src = "src/awdl/helper.c";
    println!("cargo:rerun-if-changed={src}");
    let out =
        std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("airhand-awdl");
    let compiler = cc::Build::new()
        .std("gnu11")
        .opt_level(2)
        .debug(false)
        .warnings(true)
        .get_compiler();
    let status = compiler
        .to_command()
        .arg(src)
        .arg("-o")
        .arg(&out)
        // 不要调试信息、本地符号：只是一个很小的守护进程
        .args(["-Wl,-dead_strip", "-Wl,-S", "-Wl,-x"])
        .status()
        .expect("run the C compiler for the AWDL helper");
    assert!(status.success(), "compiling {src} failed");
}

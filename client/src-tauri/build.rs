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

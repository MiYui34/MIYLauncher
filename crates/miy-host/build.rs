use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=src/sehim/shim.c");
    println!("cargo:rerun-if-changed=src/sehim/shim.h");

    // 静态 CRT 必须和 Rust 侧一致，否则同一个进程里两套 CRT，
    // 跨边界的 malloc/free 会当场炸。
    cc::Build::new()
        .file("src/sehim/shim.c")
        .static_crt(true)
        .warnings(false)
        .compile("miy_sehim");

    embed_icon();
    compress_assets();
}

/// 把界面 HTML 压成 gzip 放进 OUT_DIR，运行时由 `src/assets.rs` 解开。
///
/// 这些文件加起来两百多 KB，直接 `include_str!` 会原样占进 exe；压过之后
/// 只剩四分之一左右，而解压只在第一次打开对应页面时发生。
fn compress_assets() {
    use std::io::Write;

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(&manifest_dir);
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    let files: [(&str, PathBuf); 8] = [
        ("ui.html", manifest_dir.join("src").join("ui.html")),
        ("intro.html", manifest_dir.join("src").join("intro.html")),
        ("home.html", workspace.join("plugins/miy-mc/ui/home.html")),
        ("versions.html", workspace.join("plugins/miy-mc/ui/versions.html")),
        ("games.html", workspace.join("plugins/miy-mc/ui/games.html")),
        ("mods.html", workspace.join("plugins/miy-mc/ui/mods.html")),
        ("detail.html", workspace.join("plugins/miy-mc/ui/detail.html")),
        ("modlist.html", workspace.join("plugins/miy-mc/ui/modlist.html")),
    ];

    for (name, path) in files {
        println!("cargo:rerun-if-changed={}", path.display());
        let Ok(raw) = std::fs::read(&path) else {
            println!("cargo:warning=找不到界面文件：{}", path.display());
            continue;
        };
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        if enc.write_all(&raw).is_err() {
            continue;
        }
        let Ok(gz) = enc.finish() else { continue };
        let dest = out_dir.join(format!("{name}.gz"));
        let _ = std::fs::write(&dest, &gz);
    }
}

/// 把 `docs/icon/miy.ico` 编成 exe 的 RT_GROUP_ICON。
///
/// 不引第三方 crate：Windows SDK 自带 `rc.exe`，直接调它比多一个构建依赖划算。
/// 找不到 rc.exe 就跳过（只影响资源管理器里的图标，不影响程序本身）。
fn embed_icon() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace = manifest_dir.parent().and_then(|p| p.parent()).unwrap_or(&manifest_dir);
    let ico = workspace.join("docs").join("icon").join("miy.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    if !ico.exists() {
        println!("cargo:warning=图标不存在，跳过嵌入：{}", ico.display());
        return;
    }

    let Some(rc) = find_rc() else {
        println!("cargo:warning=找不到 rc.exe（Windows SDK），跳过图标嵌入");
        return;
    };

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let rc_file = out_dir.join("miy_icon.rc");
    let res_file = out_dir.join("miy_icon.res");

    // rc.exe 认识正斜杠，省得在 .rc 里写 \\ 转义。
    let icon_path = ico.display().to_string().replace('\\', "/");
    let script = format!("1 ICON \"{icon_path}\"\n");
    if std::fs::write(&rc_file, script).is_err() {
        println!("cargo:warning=写临时 .rc 失败，跳过图标嵌入");
        return;
    }

    let ok = Command::new(&rc)
        .arg("/nologo")
        .arg(format!("/fo{}", res_file.display()))
        .arg(&rc_file)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        println!("cargo:warning=rc.exe 编译图标失败，跳过嵌入");
        return;
    }

    println!("cargo:rustc-link-arg={}", res_file.display());
}

fn find_rc() -> Option<PathBuf> {
    // 1) PATH 里
    if let Ok(out) = Command::new("where").arg("rc.exe").output() {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(first) = text.lines().next() {
                let p = PathBuf::from(first.trim());
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }

    // 2) 扫 Windows Kits（取最高版本的 x64\rc.exe）
    let kits = Path::new("C:\\Program Files (x86)\\Windows Kits\\10\\bin");
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(kits)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("x64").join("rc.exe"))
        .filter(|p| p.exists())
        .collect();
    candidates.sort();
    candidates.pop()
}

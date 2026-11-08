//! 编译期把界面资源压成 gzip，运行时再解开。
//!
//! 这些 HTML 加起来两百多 KB，占 exe 体积的一成多；gzip 之后只剩四分之一左右。
//! 解压只在第一次用到某个页面时发生（几毫秒），之后走 `OnceLock` 缓存。

use std::sync::OnceLock;

macro_rules! asset {
    ($name:ident, $file:literal) => {
        pub fn $name() -> &'static str {
            static CELL: OnceLock<String> = OnceLock::new();
            CELL.get_or_init(|| {
                let raw = include_bytes!(concat!(env!("OUT_DIR"), "/", $file, ".gz"));
                let mut out = String::new();
                let mut dec = flate2::read::GzDecoder::new(&raw[..]);
                use std::io::Read;
                match dec.read_to_string(&mut out) {
                    Ok(_) => out,
                    Err(e) => {
                        // 解不开说明构建产物坏了，给一个能看懂的占位页而不是白屏。
                        format!(
                            "<!doctype html><meta charset=\"utf-8\"><body style=\"font:14px/1.6 'Segoe UI',sans-serif;padding:32px\">\
                             界面资源解压失败：{e}。请重新安装启动器。</body>"
                        )
                    }
                }
            })
        }
    };
}

asset!(ui, "ui.html");
asset!(intro, "intro.html");
asset!(home, "home.html");
asset!(versions, "versions.html");
asset!(games, "games.html");
asset!(mods, "mods.html");
asset!(detail, "detail.html");
asset!(modlist, "modlist.html");

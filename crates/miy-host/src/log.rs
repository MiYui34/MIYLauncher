//! 落盘日志 —— 进程崩了之后唯一还活着的证据。
//!
//! 没有日志的启动器等于没有黑匣子的飞机：用户说"点开始游戏没反应"，
//! 你手里只有一句描述。所以从骨架这一步起就写日志，不等以后再补。
//!
//! 简单到不会出错：单文件、追加、超 1 MiB 轮转一次（`.log` → `.1.log`）。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

const MAX_BYTES: u64 = 1024 * 1024;

/// 0 = ERROR，1 = WARN，2 = INFO，3 = DEBUG。
/// 默认 INFO：默认就把每次调用都记下来只会把有用的行淹掉。
static MIN_LEVEL: AtomicU8 = AtomicU8::new(2);

/// 设置系统读到 `core.log.level` 后调这里。认不出的值回落到 INFO。
pub fn set_min_level(name: &str) {
    let lvl = match name.trim().to_ascii_lowercase().as_str() {
        "error" => 0,
        "warn" | "warning" => 1,
        "debug" | "trace" => 3,
        _ => 2,
    };
    MIN_LEVEL.store(lvl, Ordering::Relaxed);
}

fn rank(level: &str) -> u8 {
    match level {
        "ERROR" => 0,
        "WARN" => 1,
        "INFO" => 2,
        _ => 3,
    }
}

static START: OnceLock<Instant> = OnceLock::new();
static SINK: OnceLock<Mutex<Option<File>>> = OnceLock::new();
static PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn init() {
    let _ = START.set(Instant::now());
    let dir = crate::paths::logs_dir();
    let _ = fs::create_dir_all(&dir);
    let file = dir.join("launcher.log");

    // 轮转：只留上一代。日志是用来救火的，不是用来考古的。
    if fs::metadata(&file).map(|m| m.len()).is_ok_and(|n| n > MAX_BYTES) {
        let _ = fs::remove_file(dir.join("launcher.1.log"));
        let _ = fs::rename(&file, dir.join("launcher.1.log"));
    }

    let sink = OpenOptions::new().create(true).append(true).open(&file).ok();
    let _ = PATH.set(file);
    let _ = SINK.set(Mutex::new(sink));

    // panic 钩子：宿主编的是 panic=abort，没有 unwind 可抓，但钩子仍会在中止
    // 之前跑一次 —— 那一次就是"死前最后一句话"，必须落盘。
    std::panic::set_hook(Box::new(|info| {
        let where_ = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "未知位置".to_string());
        let what = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "（没有消息）".to_string());
        write("ERROR", &format!("宿主 panic @ {where_}：{what}"));
    }));
}

/// 日志文件路径（给"打开日志"按钮用）。
pub fn path() -> Option<PathBuf> {
    PATH.get().cloned()
}

pub fn write(level: &str, msg: &str) {
    if rank(level) > MIN_LEVEL.load(Ordering::Relaxed) {
        return;
    }
    let ms = START.get().map_or(0, |t| t.elapsed().as_millis());
    let line = format!("[{:>9.3}s] {:<5} {}\n", ms as f64 / 1000.0, level, msg);

    // 有控制台就同步一份：开发时 eye 比 file 快
    match level {
        "ERROR" | "WARN" => eprint!("{line}"),
        _ => print!("{line}"),
    }

    if let Some(lock) = SINK.get() {
        if let Ok(mut guard) = lock.lock() {
            if let Some(f) = guard.as_mut() {
                // 宁可慢一点点，也要保证崩溃前那几行已经在盘上
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
            }
        }
    }
}

pub fn info(msg: &str) {
    write("INFO", msg);
}
pub fn warn(msg: &str) {
    write("WARN", msg);
}
pub fn error(msg: &str) {
    write("ERROR", msg);
}
pub fn debug(msg: &str) {
    write("DEBUG", msg);
}

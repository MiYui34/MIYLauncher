//! 路径策略：**启动器碰得到的所有文件都在 exe 旁边的 `MIYLauncher\` 里**。
//!
//! 早先的版本把数据写进 `%LOCALAPPDATA%`，后来虽然默认改成跟着 exe 走，但仍留了
//! 一条"目录不可写就退回 `%LOCALAPPDATA%`"的退路。那条退路会把用户的数据悄悄
//! 分散到两个地方：同一个启动器，一半配置在 D 盘，一半在 C 盘，拷走文件夹就少一半。
//! 现在没有退路了 —— 目录写不了就直说，让用户把程序挪到能写的地方，而不是偷偷换地方。
//!
//! 只有一处例外，而且是**只读**的：老版本（把数据放在 `%LOCALAPPDATA%` 的那批）
//! 升级上来时，会把配置和账号从旧目录**复制**到新目录，旧目录一个字节都不动。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// 用户数据根目录：`<exe 目录>\MIYLauncher`。
///
/// **没有备用位置。** 写不进去就让它写不进去，启动时会把错误摆到界面上；
/// 换到 `%LOCALAPPDATA%` 只是把问题藏起来，代价是用户的数据被劈成两半。
pub fn data_root() -> PathBuf {
    ROOT.get_or_init(resolve_root).clone()
}

fn resolve_root() -> PathBuf {
    let beside = exe_dir().join("MIYLauncher");
    if !writable(&beside) {
        // 装到 Program Files 之类只读的位置了。这里只提醒，不改路径：
        // 界面照常起来，用户能看到"目录不可写"，自己把 exe 挪到别处即可。
        eprintln!(
            "[paths] {} 不可写。启动器不会把数据放到任何别的地方，请把程序移到可写目录（例如桌面）。",
            beside.display()
        );
    }
    beside
}

/// 能不能在这个目录里创建/写入。用真实写盘探测，不靠只读属性猜。
fn writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".write-probe");
    match std::fs::write(&probe, b"1") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 老版本的数据目录（`%LOCALAPPDATA%\MIYLauncher`），只用于一次性迁移。
pub fn legacy_root() -> Option<PathBuf> {
    let old = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?.join("MIYLauncher");
    if old.is_dir() && old != data_root() {
        Some(old)
    } else {
        None
    }
}

/// 把老目录里**小而重要**的东西搬过来：配置、凭据、窗口位置。
///
/// 故意不搬 `assets/`（几百 MB）、`libraries/` 和 `versions/` —— 那些要么能重建，
/// 要么用户已经在别处有一份，一次"搬家"把磁盘写满是更糟的体验。
pub fn migrate_legacy() {
    let Some(old) = legacy_root() else { return };
    let new = data_root();
    let mut moved = Vec::new();
    for rel in ["config/settings.json", "config/auth.json", "config/offline.json", "window.json"] {
        let from = old.join(rel);
        let to = new.join(rel);
        if !from.is_file() || to.exists() {
            continue;
        }
        if let Some(parent) = to.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::copy(&from, &to).is_ok() {
            moved.push(rel);
        }
    }
    // 账号（DPAPI 密文）也搬，否则用户得重新登录一次。
    let acc_from = old.join("accounts");
    let acc_to = new.join("accounts");
    if acc_from.is_dir() && !acc_to.exists() && std::fs::create_dir_all(&acc_to).is_ok() {
        if let Ok(rd) = std::fs::read_dir(&acc_from) {
            for e in rd.flatten() {
                let to = acc_to.join(e.file_name());
                if std::fs::copy(e.path(), &to).is_ok() {
                    moved.push("accounts");
                }
            }
        }
    }
    if !moved.is_empty() {
        eprintln!("[paths] 已从 {} 迁移：{}", old.display(), moved.join(", "));
    }
}

pub fn webview_dir() -> PathBuf {
    data_root().join("webview2")
}

/// 自动下载的 Java（Mojang 的 java-runtime）装在这里，和游戏文件一起待在 exe 旁边。
pub fn java_dir() -> PathBuf {
    data_root().join("java")
}

/// 临时文件（下载中断留下的 `.part`、解压中间产物）也只落在这里。
pub fn temp_dir() -> PathBuf {
    data_root().join("temp")
}

pub fn logs_dir() -> PathBuf {
    data_root().join("logs")
}

/// 设置文件的位置：`<data>/config/settings.json`。
pub fn config_file() -> PathBuf {
    data_root().join("config").join("settings.json")
}

/// 老版本把它放在 `<data>/config.json`，留着只为了迁移一次。
pub fn legacy_config_file() -> PathBuf {
    data_root().join("config.json")
}

pub fn config_dir() -> PathBuf {
    data_root().join("config")
}

/// 私有凭据（Microsoft 客户端 ID）放这里，**不进设置、不进界面**。
pub fn auth_file() -> PathBuf {
    config_dir().join("auth.json")
}

/// 离线身份（玩家名 / UUID），用户可自己改。
pub fn offline_file() -> PathBuf {
    config_dir().join("offline.json")
}

/// 程序目录（exe 所在目录）。
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 插件目录：优先 exe 旁边的 `plugins/`，开发时退到当前目录。
pub fn plugins_dir() -> PathBuf {
    let beside = exe_dir().join("plugins");
    if beside.is_dir() {
        return beside;
    }
    // `cwd\plugins` 只服务开发期（cargo run / 直接跑 target\release 下的 exe）。
    // 发行版只认 exe 旁边：否则用户在任意目录双击 exe，只要那个目录里恰好有个
    // `plugins\`，就会被装进一堆不相干的插件，启动即报错。
    if is_dev_build() {
        let cwd = std::env::current_dir().unwrap_or_default().join("plugins");
        if cwd.is_dir() {
            return cwd;
        }
    }
    beside
}

/// exe 是否躺在 `target\{debug,release}` 里 —— 只有这种布局才算开发期。
fn is_dev_build() -> bool {
    let exe = std::env::current_exe().unwrap_or_default();
    let dir = match exe.parent() {
        Some(d) => d,
        None => return false,
    };
    let profile_ok = dir
        .file_name()
        .map(|n| n.eq_ignore_ascii_case("debug") || n.eq_ignore_ascii_case("release"))
        .unwrap_or(false);
    let under_target = dir
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.eq_ignore_ascii_case("target"))
        .unwrap_or(false);
    profile_ok && under_target
}

/// 建好所有可写目录。失败不是致命错——只是日志和缓存落不了盘，
/// 但主界面还得能起来，所以调用方只记日志、不退出。
pub fn ensure_writable_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(data_root())?;
    std::fs::create_dir_all(logs_dir())?;
    std::fs::create_dir_all(webview_dir())?;
    std::fs::create_dir_all(config_dir())?;
    std::fs::create_dir_all(java_dir())?;
    std::fs::create_dir_all(temp_dir())?;
    migrate_legacy();
    Ok(())
}

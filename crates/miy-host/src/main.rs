//! 宿主入口。
//!
//! 顺序刻意如此，每一步都在替后面兜底：
//!
//! 1. `mark_start` —— 计时从第一行开始，首帧指标的分母就是这个。
//! 2. `log::init` —— 后面所有步骤都可能失败，得先有地方记。
//! 3. `install_process_defaults` —— 先把 SEH 和 DLL 搜索路径钉死。
//! 4. 目录 —— 一切跟着 exe 走：数据在 `<exe 目录>\MIYLauncher`，游戏在 `<exe 目录>\MC`，
//!    没有 `%LOCALAPPDATA%` / `%TEMP%` 之类的备用位置。
//! 5. 总线 + 宿主 ABI —— 插件的一切都要经过它。
//! 6. 单实例 —— 第二个进程只负责把已有窗口叫到前台。
//! 7. 插件在**后台线程**加载 —— 冷启动要 200+ ms，不能挡住首帧。
//! 8. 最后才开窗口。

// 发行版用 GUI 子系统：从资源管理器双击时不该先弹一个黑色控制台窗口。
// 但 `--spike` / `--login-probe` 这些探针要靠 stdout 给人看，所以 main 第一件事
// 是带参数启动时挂回父进程的控制台（见 `attach_console`）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod accounts;
mod assets;
mod bus;
mod clipboard;
mod embedded;
mod heap;
mod ipc;
mod log;
mod net;
mod paths;
mod seh;
mod settings;
mod spike;
mod ui;

use std::time::{Duration, Instant};

use serde_json::json;
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

/// 互斥体句柄必须活到进程结束；`CloseHandle` 会让"已有实例"判断失效。
static INSTANCE_HANDLE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// 把 stdout/stderr 接回**父进程**的控制台。
///
/// 发行版是 GUI 子系统（`windows_subsystem = "windows"`），双击启动不会带控制台；
/// 从终端跑 `MIYLauncher.exe --spike` 时父进程已经有控制台，`AttachConsole`
/// 认领它，再把 stdout 指向 `CONOUT$`，`println!` 才有地方去。
///
/// 双击启动（没有参数）直接返回 —— 探针才需要输出，正常启动只写日志文件。
fn attach_console() {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    if std::env::args_os().len() <= 1 {
        return;
    }
    unsafe {
        // 已经有可用的 stdout（被重定向到管道/文件，或本来就继承了控制台）时什么
        // 都别做：`AttachConsole` + `CONOUT$` 会把调用方给的那根有效句柄顶掉，
        // 输出就凭空消失了。
        let cur = GetStdHandle(STD_OUTPUT_HANDLE as u32);
        if !cur.is_null() && cur != INVALID_HANDLE_VALUE {
            return;
        }
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        // UTF-16 常量要活到调用结束；这里直接写死名字更省事。
        let name: [u16; 8] = [
            b'C' as u16, b'O' as u16, b'N' as u16, b'O' as u16, b'U' as u16, b'T' as u16,
            b'$' as u16, 0,
        ];
        for std_id in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let h = CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if h != INVALID_HANDLE_VALUE {
                SetStdHandle(std_id as u32, h);
            }
        }
    }
}

fn main() {
    attach_console();
    ui::mark_start();
    log::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |a: &str| args.iter().any(|x| x == a);

    if has("--help") || has("-h") {
        print_help();
        std::process::exit(0);
    }
    if has("--version") || has("-V") {
        println!("MIYLauncher {} (abi {}.{})", env!("CARGO_PKG_VERSION"), 1, 2);
        std::process::exit(0);
    }
    // 不认识的参数不要装没看见：typo 的 `--safe_mode` 会静默变成正常启动。
    for a in &args {
        // 只对"看起来像开关"的参数告警：--name 26.3 里的值是正常输入。
        if !a.starts_with('-') {
            continue;
        }
        if !matches!(
            a.as_str(),
            "--spike"
                | "--check-webview"
                | "--ui-probe"
                | "--safe-mode"
                | "--no-plugins"
                | "--portable"
                | "--net-probe"
        | "--net-dl"
                | "--mc-probe"
                | "--mc-install"
                | "--mc-launch"
                | "--mc-args"
                | "--mc-assets"
        | "--mc-repair"
                | "--loader"
                | "--loader-version"
                | "--id"
                | "--call"
                | "--body"
                | "--full"
                | "--name"
                | "--login-probe"
                | "--login-flow"
                | "--drop-test"
                | "--help"
                | "-h"
                | "--version"
                | "-V"
        ) {
            log::warn(&format!("忽略未知参数: {a}"));
        }
    }
    let safe_mode = has("--safe-mode") || has("--no-plugins");

    unsafe { seh::install_process_defaults() };

    if let Err(e) = paths::ensure_writable_dirs() {
        // 这里不再退回 `%LOCALAPPDATA%`：把数据放到别处，用户的存档、设置和插件
        // 就会莫名其妙地分成两半。宁可当场说清楚「换个可写目录」。
        let msg = format!(
            "启动器目录不可写：{e}\n\n程序不会把任何文件放到这个文件夹以外的地方。\n\
             请把 MIYLauncher.exe 移到可写的目录（例如桌面、或 D 盘上新建的文件夹）再运行。"
        );
        log::error(&msg);
        if args.is_empty() {
            ui::fatal(&msg);
        }
        std::process::exit(1);
    }

    // 设置要早于一切"会打日志的东西"加载：日志级别本身就是个设置项。
    settings::init();
    if let Some(level) = settings::effective_str("core.log.level") {
        log::set_min_level(&level);
    }
    // 账号只读磁盘、不联网：上次登过就先认下来，令牌等到真要启动游戏再换。
    accounts::init();

    log::info(&format!(
        "MIYLauncher {} 已启动。数据目录 {}，插件目录 {}",
        env!("CARGO_PKG_VERSION"),
        paths::data_root().display(),
        paths::plugins_dir().display()
    ));

    bus::init();
    bus::init_host_api();
    register_host_services();
    // 内嵌的核心插件（miy.mc）先装：它不读盘、不 LoadLibrary，几乎不花时间，
    // 但界面一打开就要用到它的设置项和页面，等不了后台线程。
    load_builtin_plugins();

    // 上一次是崩着退出的？把话带到界面上，别让用户以为是自己操作错了。
    if let Some(msg) = seh::take_pending_crash() {
        log::warn(&msg);
        bus::get().emit("host.error", msg.as_bytes());
    }

    // 自检与探测都不需要窗口，但需要总线和插件目录就绪。
    if has("--spike") {
        let code = spike::run();
        bus::get().shutdown_all();
        std::process::exit(code);
    }
    if has("--check-webview") {
        std::process::exit(ui::check_webview_report());
    }
    if has("--net-probe") {
        let code = net_probe();
        std::process::exit(code);
    }
    if let Some(url) = arg_value("--net-dl") {
        let code = net_dl(&url);
        std::process::exit(code);
    }
    // 三个开关都是同一套探针：`--mc-probe` 只看，`--mc-install` 顺手装，`--mc-launch` 再启动。
    // 任何一个单独出现都要走这里，否则会掉进 UI 分支。放了三种写法：`--mc-install`
    // 单用（CI 里最顺手）、`--mc-probe --mc-install`、以及全都要。
    if has("--mc-probe") || has("--mc-install") || has("--mc-launch") || has("--mc-args") || has("--mc-assets") || has("--mc-repair") {
        let code = mc_probe(
            has("--mc-probe"),
            has("--mc-install"),
            has("--mc-launch"),
            has("--mc-args"),
            has("--mc-assets"),
            has("--mc-repair"),
        );
        std::process::exit(code);
    }
    if has("--login-probe") {
        std::process::exit(login_probe());
    }
    if has("--login-flow") {
        std::process::exit(login_flow());
    }
    // 不建窗口直接打一个服务：排查「页面取不到 / 服务不通」比重开界面快，
    // 也是内置插件（已经编进 exe、没有 dll 可单跑）的验收入口。
    if let Some(svc) = arg_value("--call") {
        std::process::exit(call_probe(&svc));
    }

    if let Some(code) = single_instance_guard() {
        std::process::exit(code);
    }

    if safe_mode {
        // 安全模式的价值全在"插件是嫌疑犯"这个场景：宿主自己别跟着陪葬。
        log::warn("安全模式：本次不加载任何插件");
        bus::get().emit(
            "host.error",
            "安全模式：本次没有加载任何插件。确认问题后再正常启动即可恢复。".as_bytes(),
        );
    } else {
        spawn_plugin_loader();
    }

    let code = ui::run(has("--ui-probe"));
    bus::get().shutdown_all();
    log::info("退出");
    std::process::exit(code);
}

// ---- 宿主自己的服务（`miy.app.*`） -----------------------------------------

fn print_help() {
    println!(
        "MIYLauncher {} — 单 exe，一切皆插件\n\
         \n\
         用法: MIYLauncher.exe [选项]\n\
         \n\
         选项:\n\
         \x20 --safe-mode, --no-plugins   不加载任何插件启动（怀疑插件把宿主搞崩时用）\n\
         \x20 --portable                  数据写到 exe 旁边的 data\\ 里\n\
         \x20 --check-webview             只探测 WebView2 运行时，不建窗口\n\
         \x20 --ui-probe                  启动窗口、2.5 秒后自动关闭，打印首帧指标\n\
         \x20 --spike                     运行自检（插件加载/调用/崩溃隔离/设置/任务）\n\
         \x20 --mc-assets                 只补资源文件，不启动游戏
        \x20 --mc-repair                 按大小与 sha1 校验并补回缺失文件
         \x20 --login-probe               申请一次设备码并轮询一次（验正版登录链路）\n\
         \x20 --login-flow                走完一整次设备码登录并逐步打印（排查登录问题首选）
         \x20 --call <服务> [--body <json>] [--full]
         \x20                             不建窗口直接调一个服务（查页面/查插件服务用）
         \x20 --version                   打印版本与 ABI\n\
         \x20 --help                      这份说明\n\
         \n\
         用户数据: {}\n\
         日志:     {}",
        env!("CARGO_PKG_VERSION"),
        paths::data_root().display(),
        paths::logs_dir().join("launcher.log").display()
    );
}

/// `--login-probe`：只验正版登录链路的前两步（申请设备码 + 轮询一次）。
///
/// 真正完成登录必须有人在浏览器里点同意，所以这里不假装成功——它只回答
/// "配置对不对、微软那边通不通、错误翻没翻成人话"。
fn login_probe() -> i32 {
    // 客户端 ID 现在是凭据，只从私有文件/环境变量来；这里连打码后的样子都
    // 只在需要排查时打——探针的输出经常被贴到对话里。
    let has = accounts::client_id_hint();
    if !has.has {
        println!("[login] 还没配置 Microsoft 客户端 ID（用户数据目录下 config/auth.json）");
        return 2;
    }
    println!("[login] client id: {}（已配置，已隐藏）", has.masked);
    // 先自检：这一步不碰浏览器，专门用来把「端点选错 / 重定向没配 /
    // 公共客户端流没开」这类配置问题当场定位出来。
    let chk = accounts::check();
    println!(
        "[login] 自检: {}，{}",
        chk.get("verdict").and_then(|x| x.as_str()).unwrap_or("?"),
        chk.get("message").and_then(|x| x.as_str()).unwrap_or("?")
    );
    if let Some(d) = chk.get("detail").and_then(|x| x.as_str()) {
        if !d.is_empty() {
            println!("[login] 自检细节: {d}");
        }
    }
    println!(
        "[login] 当前状态: {}",
        if accounts::is_logged_in() { "已登录" } else { "未登录" }
    );

    match accounts::start() {
        Ok(v) => {
            let code = v.get("user_code").and_then(|x| x.as_str()).unwrap_or("?");
            let uri = v.get("verify_uri").and_then(|x| x.as_str()).unwrap_or("?");
            println!("[login] 设备码 {code}，请在 {uri} 输入");
            match accounts::poll() {
                Ok(p) => {
                    let st = p.get("status").and_then(|x| x.as_str()).unwrap_or("?");
                    println!("[login] 第一次轮询: {st}");
                    accounts::cancel();
                    0
                }
                Err(e) => {
                    println!("[login] 轮询失败: {e}");
                    1
                }
            }
        }
        Err(e) => {
            println!("[login] 启动失败: {e}");
            1
        }
    }
}

/// `--login-flow`：在终端里走完一整次设备码登录，把每一步都打出来。
///
/// 界面上出问题时最有用的一条命令：它和界面走的是同一套 `accounts::*`，
/// 但从申请码、打开浏览器、每次轮询的 HTTP 结果、到最后换 MC 身份，
/// 全程有输出，不用猜是"没在轮询"还是"微软说还没授权"。
fn login_flow() -> i32 {
    let has = accounts::client_id_hint();
    if !has.has {
        println!("[flow] 还没配置 Microsoft 客户端 ID（用户数据目录下 config/auth.json）");
        return 2;
    }
    println!("[flow] client id: {}（已配置，已隐藏）", has.masked);

    let chk = accounts::check();
    println!(
        "[flow] 自检: {}，{}",
        chk.get("verdict").and_then(|x| x.as_str()).unwrap_or("?"),
        chk.get("message").and_then(|x| x.as_str()).unwrap_or("?")
    );
    if chk.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        println!("[flow] 自检没过，先修配置再来");
        return 2;
    }

    let v = match accounts::start() {
        Ok(v) => v,
        Err(e) => {
            println!("[flow] 申请设备码失败: {e}");
            return 1;
        }
    };
    let code = v.get("user_code").and_then(|x| x.as_str()).unwrap_or("?");
    let uri = v.get("verify_uri").and_then(|x| x.as_str()).unwrap_or("?");
    println!("[flow] 设备码 {code}");
    println!("[flow] 打开 {uri} 输入上面的码并同意");
    let _ = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", uri])
        .spawn();

    // 每 2 秒问一次，最多等 15 分钟（和码的有效期一致）。
    let deadline = std::time::Instant::now() + Duration::from_secs(900);
    let mut n = 0;
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(2));
        n += 1;
        match accounts::poll() {
            Ok(p) => {
                let st = p.get("status").and_then(|x| x.as_str()).unwrap_or("?").to_string();
                if st != last {
                    println!("[flow] 轮询 #{n}: {st} {}", p);
                    last = st.clone();
                } else {
                    println!("[flow] 轮询 #{n}: {st}");
                }
                match st.as_str() {
                    "ok" => {
                        let name = p.get("name").and_then(|x| x.as_str()).unwrap_or("?");
                        println!("[flow] 登录成功：{name}");
                        return 0;
                    }
                    "expired" => {
                        println!("[flow] 码过期了");
                        return 3;
                    }
                    "declined" => {
                        println!("[flow] 授权被拒");
                        return 3;
                    }
                    "idle" => {
                        println!("[flow] 会话已结束（可能已被别处取消）");
                        return 3;
                    }
                    _ => {}
                }
            }
            Err(e) => {
                println!("[flow] 轮询出错: {e}");
                return 1;
            }
        }
    }
    println!("[flow] 等太久了，放弃");
    accounts::cancel();
    3
}

fn net_dl(url: &str) -> i32 {
    // 落盘位置必须在自己目录里：启动器不在用户机器上留任何散落文件，
    // 连调试命令也不例外（`%TEMP%` 也算别人的地盘）。
    let dir = paths::data_root().join("cache");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("net-dl-test.bin");
    let started = Instant::now();
    match net::download(url, &path, Duration::from_secs(600)) {
        Ok(n) => {
            let ms = started.elapsed().as_millis();
            let disk = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            println!("[net] download -> {n} B / 磁盘 {disk} B / {ms} ms / {}", path.display());
            0
        }
        Err(e) => {
            println!("[net] download 失败：{e}");
            1
        }
    }
}

fn net_probe() -> i32 {
    let urls = [
        "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json",
        "https://launchermeta.mojang.com/mc/game/version_manifest_v2.json",
    ];
    let mut code = 1;
    for url in urls {
        let started = Instant::now();
        match net::get_limited(url, Duration::from_secs(20), 8 * 1024 * 1024) {
            Ok(r) => {
                let ms = started.elapsed().as_millis();
                println!("[net] {url} -> HTTP {} / {} B / {ms} ms", r.status, r.body.len());
                if r.status == 200 {
                    match serde_json::from_slice::<serde_json::Value>(&r.body) {
                        Ok(v) => {
                            let latest = v
                                .pointer("/latest/release")
                                .and_then(|x| x.as_str())
                                .unwrap_or("?");
                            let n = v
                                .get("versions")
                                .and_then(|x| x.as_array())
                                .map_or(0, |a| a.len());
                            println!("[net] 最新正式版 {latest}，清单里 {n} 个版本");
                            log::info(&format!("net-probe OK：最新 {latest}，{n} 个版本"));
                            code = 0;
                        }
                        Err(e) => println!("[net] 响应不是 JSON：{e}"),
                    }
                }
            }
            Err(e) => println!("[net] {url} 失败：{e}"),
        }
        if code == 0 {
            break;
        }
    }
    code
}

/// `--call <service> [--body <json>] [--full]`：命令行直接打一个服务。
fn call_probe(service: &str) -> i32 {
    for r in &bus::load_all(&paths::plugins_dir()) {
        if let Err(e) = r {
            println!("[call] 插件加载失败：{e}");
        }
    }
    let body = arg_value("--body").unwrap_or_else(|| "{}".to_string());
    // `--repeat N`：同一个进程里连调 N 次，用来验证插件侧的内存缓存是否生效
    // （每次 `--call` 都是新进程，缓存活不过一次调用）。
    let repeat: usize = arg_value("--repeat")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .clamp(1, 20);
    let mut last: Vec<u8> = Vec::new();
    for i in 0..repeat {
        let started = Instant::now();
        // 装原版 / 跑 Forge 安装器都是分钟级，探针别用 120 秒卡死自己。
        let out = bus::get().call(service, body.as_bytes(), Duration::from_secs(900));
        match out {
            Ok(v) => {
                println!(
                    "[call] {service} 第 {} 次用时 {} ms，返回 {} 字节",
                    i + 1,
                    started.elapsed().as_millis(),
                    v.len()
                );
                last = v;
            }
            Err(code) => {
                println!("[call] {service} 失败，错误码 {code}");
                bus::get().shutdown_all();
                return 1;
            }
        }
    }
    bus::get().shutdown_all();
    {
        let v = last;
        // 页面正文动辄几十 KB，默认只给一段预览，`--full` 才全量打印。
        let full = std::env::args().any(|a| a == "--full");
        let n = if full { v.len() } else { v.len().min(600) };
        println!("{}", String::from_utf8_lossy(&v[..n]));
    }
    0
}

/// `--mc-probe`：把插件真的加载起来，跑一遍「版本清单 → 版本解析」。
///
/// 这是第一个真实插件链的无窗口验收：不需要点界面就能看出网络、规则过滤、
/// inheritsFrom 合并、运行库清单四件事是否都对。
/// 取 `--flag value` 里的值（只给调试探针用，正常启动不看这些）。
fn arg_value(flag: &str) -> Option<String> {
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == flag {
            return it.next();
        }
    }
    None
}

fn mc_probe(probe: bool, install: bool, launch: bool, dry: bool, want_assets: bool, repair: bool) -> i32 {
    let results = bus::load_all(&paths::plugins_dir());
    for r in &results {
        if let Err(e) = r {
            println!("[mc] 插件加载失败：{e}");
        }
    }
    // miy.mc 是内置的（编进 exe），不在磁盘加载结果里，所以看注册表。
    let snapshot = bus::get().snapshot();
    println!(
        "[mc] 插件 {} 个在册（磁盘 {} 个）",
        snapshot.len(),
        results.len()
    );
    let has_mc = snapshot.iter().any(|p| p.id == "miy.mc" && p.started);
    if !has_mc {
        println!("[mc] 没找到 miy.mc 插件");
        bus::get().shutdown_all();
        return 1;
    }
    let b = bus::get();

    // 指定了 --name 就不用去拉清单：镜像挂了也能验已装版本。
    // `--id` 单独指定版本号，`--name` 只当目录名（装加载器版本时：`--id 26.1.2 --name 26.1.2-fabric`）。
    let id_flag = arg_value("--id");
    let named = id_flag.clone().or_else(|| arg_value("--name"));
    let started = Instant::now();
    let versions = if named.is_some() {
        b"{}".to_vec()
    } else {
        match b.call("miy.mc.versions", b"{}", Duration::from_secs(40)) {
            Ok(v) => v,
            Err(code) => {
                println!("[mc] versions 失败，错误码 {code}");
                b.shutdown_all();
                return 1;
            }
        }
    };
    let ms = started.elapsed().as_millis();
    let v: serde_json::Value = match serde_json::from_slice(&versions) {
        Ok(v) => v,
        Err(e) => {
            println!("[mc] versions 不是 JSON：{e}");
            b.shutdown_all();
            return 1;
        }
    };
    let latest = match &named {
        Some(n) => n.clone(),
        None => v
            .pointer("/latest/release")
            .and_then(|x| x.as_str())
            .unwrap_or("?")
            .to_string(),
    };
    let count = v.get("count").and_then(|x| x.as_u64()).unwrap_or(0);
    let listed = v
        .get("versions")
        .and_then(|x| x.as_array())
        .map(Vec::len)
        .unwrap_or(0);
    let april = v
        .get("versions")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter(|x| x.get("april").and_then(|b| b.as_bool()).unwrap_or(false))
                .count()
        })
        .unwrap_or(0);
    if probe {
        println!(
            "[mc] versions {ms} ms，最新正式版 {latest}，清单 {count} 个版本（返回 {listed} 条，愚人节 {april} 个）"
        );
    }

    let req = json!({ "id": latest }).to_string();
    let started = Instant::now();
    let info = match b.call("miy.mc.info", req.as_bytes(), Duration::from_secs(40)) {
        Ok(v) => v,
        Err(code) => {
            println!("[mc] info 失败，错误码 {code}");
            b.shutdown_all();
            return 1;
        }
    };
    let ms = started.elapsed().as_millis();
    let i: serde_json::Value = serde_json::from_slice(&info).unwrap_or(serde_json::Value::Null);
    if i.get("ok") == Some(&serde_json::Value::Bool(false)) {
        println!("[mc] info 报错：{}", i.get("error").and_then(|e| e.as_str()).unwrap_or("?"));
        b.shutdown_all();
        return 1;
    }
    println!(
        "[mc] info {ms} ms，mainClass {}，资源 {}，运行库 {} 个，共 {} 个文件 / {:.1} MB",
        i.get("mainClass").and_then(|x| x.as_str()).unwrap_or("?"),
        i.get("assets").and_then(|x| x.as_str()).unwrap_or("?"),
        i.get("libraries").and_then(|x| x.as_u64()).unwrap_or(0),
        i.get("files").and_then(|x| x.as_u64()).unwrap_or(0),
        i.get("totalBytes").and_then(|x| x.as_u64()).unwrap_or(0) as f64 / 1_048_576.0
    );
    println!(
        "[mc] java {}，game root {}",
        i.pointer("/javaVersion/majorVersion")
            .and_then(|x| x.as_u64())
            .map(|x| x.to_string())
            .unwrap_or_else(|| "?".into()),
        i.get("root").and_then(|x| x.as_str()).unwrap_or("?")
    );

    if install {
        let name = arg_value("--name").unwrap_or_default();
        let mut body = json!({ "id": latest });
        // `--name` 是目录名：没有 `--id` 时它同时就是版本号（旧用法）。
        if !name.is_empty() {
            body["name"] = json!(name);
        }
        // 加载器跟本体一起装（和界面同一条路径），方便在命令行里验一次安装只产出一个目录。
        if let Some(l) = arg_value("--loader") {
            body["loader"] = json!(l);
            if let Some(v) = arg_value("--loader-version") {
                body["loaderVersion"] = json!(v);
            }
        }
        let req = body.to_string();
        let started = Instant::now();
        match b.call("miy.mc.install", req.as_bytes(), Duration::from_secs(600)) {
            Ok(v) => {
                let ms = started.elapsed().as_millis();
                let r: serde_json::Value = serde_json::from_slice(&v).unwrap_or_default();
                let ok = r.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
                let errs = r.get("errors").and_then(|x| x.as_array()).map_or(0, Vec::len);
                println!(
                    "[mc] install {ms} ms，{} 个文件 / {:.1} MB，失败 {errs}",
                    r.get("files").and_then(|x| x.as_u64()).unwrap_or(0),
                    r.get("bytes").and_then(|x| x.as_u64()).unwrap_or(0) as f64 / 1_048_576.0
                );
                println!(
                    "[mc] 版本目录 {}",
                    r.get("dir").and_then(|x| x.as_str()).unwrap_or("?")
                );
                if !ok {
                    for e in r
                        .get("errors")
                        .and_then(|x| x.as_array())
                        .into_iter()
                        .flatten()
                        .take(5)
                    {
                        println!("[mc]   下载失败：{}", e.as_str().unwrap_or("?"));
                    }
                }
            }
            Err(code) => println!("[mc] install 失败，错误码 {code}"),
        }
    }

    // 目标版本：`--name` 指定，缺省用清单里最新的正式版。
    let ver = arg_value("--name")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| latest.clone());

    // 资源重补：按 size + sha1 把缺的/坏的文件补回来（不删多余文件）。
    if repair {
        let req = json!({ "name": ver }).to_string();
        let started = Instant::now();
        match b.call("miy.mc.repair", req.as_bytes(), Duration::from_secs(7200)) {
            Ok(v) => {
                let r: serde_json::Value = serde_json::from_slice(&v).unwrap_or_default();
                if r.get("checked").is_none() {
                    println!(
                        "[mc] repair 报错：{}",
                        r.get("error")
                            .and_then(|x| x.as_str())
                            .or_else(|| r.get("code").and_then(|x| x.as_str()))
                            .unwrap_or("未知错误")
                    );
                } else {
                    println!(
                        "[mc] repair {} ms，检查 {} 个，缺 {} 个，补回 {} 个，仍缺 {} 个，失败 {}",
                        started.elapsed().as_millis(),
                        r.get("checked").and_then(|x| x.as_u64()).unwrap_or(0),
                        r.get("missing").and_then(|x| x.as_u64()).unwrap_or(0),
                        r.get("fixed").and_then(|x| x.as_u64()).unwrap_or(0),
                        r.get("still").and_then(|x| x.as_u64()).unwrap_or(0),
                        r.get("errors").and_then(|x| x.as_u64()).unwrap_or(0)
                    );
                }
            }
            Err(code) => println!("[mc] repair 失败，错误码 {code}"),
        }
    }

    // 资源单独可下：`--mc-assets` 只补资源不启动（修"缺贴图闪退"的现场）。
    if want_assets || (launch && !dry) {
        let req = json!({ "id": ver }).to_string();
        let started = Instant::now();
        match b.call("miy.mc.assets", req.as_bytes(), Duration::from_secs(7200)) {
            Ok(v) => {
                let r: serde_json::Value = serde_json::from_slice(&v).unwrap_or_default();
                println!(
                    "[mc] assets {} ms，{} 个对象，失败 {}",
                    started.elapsed().as_millis(),
                    r.get("files").and_then(|x| x.as_u64()).unwrap_or(0),
                    r.get("errors").and_then(|x| x.as_u64()).unwrap_or(0)
                );
            }
            Err(code) => println!("[mc] assets 失败，错误码 {code}"),
        }
    }

    if launch || dry {
        let req = json!({ "id": ver, "username": "Player", "dry": dry }).to_string();
        match b.call("miy.mc.launch", req.as_bytes(), Duration::from_secs(60)) {
            Ok(v) => {
                let r: serde_json::Value = serde_json::from_slice(&v).unwrap_or_default();
                if r.get("ok") != Some(&serde_json::Value::Bool(true)) {
                    println!(
                        "[mc] launch 报错：{}",
                        r.get("error").and_then(|x| x.as_str()).unwrap_or("?")
                    );
                } else if r.get("dry") == Some(&serde_json::Value::Bool(true)) {
                    println!(
                        "[mc] 干跑 {}，游戏目录 {}，类路径 {} 项，参数 {} 个，内存 {} MB，窗口 {}，GC {}，natives 包 {}，资源 {}",
                        r.get("id").and_then(|x| x.as_str()).unwrap_or("?"),
                        r.get("gameDir").and_then(|x| x.as_str()).unwrap_or("?"),
                        r.get("classpathEntries").and_then(|x| x.as_u64()).unwrap_or(0),
                        r.get("args").and_then(|x| x.as_u64()).unwrap_or(0),
                        r.get("memory").and_then(|x| x.as_u64()).unwrap_or(0),
                        r.get("window").and_then(|x| x.as_u64()).unwrap_or(1),
                        r.get("gc").and_then(|x| x.as_u64()).unwrap_or(0),
                        r.get("nativesJars").and_then(|x| x.as_u64()).unwrap_or(0),
                        r.get("assets").and_then(|x| x.as_str()).unwrap_or("?")
                    );
                    // 用的是哪个 Java：出「窗口一闪就没」的现场时，这是第一句要看的。
                    if let Some(j) = r.get("java").and_then(|x| x.as_str()) {
                        println!(
                            "[mc] Java {}，{}（{}）",
                            r.get("javaMajor").and_then(|x| x.as_u64()).unwrap_or(0),
                            j,
                            r.get("javaSource").and_then(|x| x.as_str()).unwrap_or("?")
                        );
                    }
                    // 启动器动过 options.txt 的话，这里把原因说清楚，别让用户以为设置被谁改了。
                    if let Some(notes) = r.get("notes").and_then(|x| x.as_array()) {
                        for n in notes {
                            if let Some(s) = n.as_str() {
                                println!("[mc]   {s}");
                            }
                        }
                    }
                    // 排查参数拼装时（自定义参数 / GC / 窗口）把整串打出来。
                    if std::env::var_os("MIY_MC_ARGV").is_some() {
                        if let Some(argv) = r.get("argv").and_then(|x| x.as_array()) {
                            for arg in argv {
                                if let Some(s) = arg.as_str() {
                                    println!("[argv] {s}");
                                }
                            }
                        }
                    }
                } else {
                    let pid = r.get("pid").and_then(|x| x.as_u64()).unwrap_or(0);
                    println!(
                        "[mc] launched pid={pid} java={}，类路径 {} 项，参数 {} 个",
                        r.get("java").and_then(|x| x.as_str()).unwrap_or("?"),
                        r.get("classpathEntries").and_then(|x| x.as_u64()).unwrap_or(0),
                        r.get("args").and_then(|x| x.as_u64()).unwrap_or(0)
                    );
                    // 盯 25 秒：把游戏自己的日志打出来，孩子还在跑就杀掉
                    let deadline = Instant::now() + Duration::from_secs(25);
                    let mut lines = 0usize;
                    let mut exited = None;
                    let mut crash_seen = false;
                    let mut exit_at: Option<Instant> = None;
                    while Instant::now() < deadline {
                        for (topic, payload) in b.drain_events(256) {
                            match topic.as_str() {
                                "mc.log" => {
                                    if lines < 24 {
                                        let v: serde_json::Value =
                                            serde_json::from_slice(&payload).unwrap_or_default();
                                        println!(
                                            "[game] {}",
                                            v.get("line").and_then(|x| x.as_str()).unwrap_or("")
                                        );
                                    }
                                    lines += 1;
                                }
                                "mc.exit" => {
                                    exited = Some(String::from_utf8_lossy(&payload).to_string());
                                    exit_at = Some(Instant::now());
                                }
                                // 崩溃窗口的数据源：原因 + 日志尾巴，探针里也打出来，
                                // 这样「游戏一闪就没」不用开界面就能看到为什么。
                                "mc.crash" => {
                                    crash_seen = true;
                                    let v: serde_json::Value =
                                        serde_json::from_slice(&payload).unwrap_or_default();
                                    println!(
                                        "[mc] 崩溃原因：{}",
                                        v.get("reason").and_then(|x| x.as_str()).unwrap_or("?")
                                    );
                                    if let Some(p) = v.get("crash").and_then(|x| x.as_str()) {
                                        if !p.is_empty() {
                                            println!("[mc] 崩溃报告：{p}");
                                        }
                                    }
                                    if let Some(log) = v.get("log").and_then(|x| x.as_str()) {
                                        for l in log.lines().rev().take(8).collect::<Vec<_>>().iter().rev() {
                                            println!("[crashlog] {l}");
                                        }
                                    }
                                }
                                "mc.assets.done" | "mc.progress" => {}
                                other => println!("[event] {other}"),
                            }
                        }
                        // 退出后再多等一会儿，让紧随其后的 mc.crash 有机会被取走。
                        if let Some(t) = exit_at {
                            if crash_seen || t.elapsed() > Duration::from_millis(1500) {
                                break;
                            }
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    match exited {
                        Some(info) => println!("[mc] 游戏已退出：{info}（日志 {lines} 行）"),
                        None => {
                            println!("[mc] 游戏仍在运行（日志 {lines} 行），结束进程 pid {pid}");
                            let _ = std::process::Command::new("taskkill")
                                .args(["/PID", &pid.to_string(), "/T", "/F"])
                                .output();
                        }
                    }
                }
            }
            Err(code) => println!("[mc] launch 失败，错误码 {code}"),
        }
    }

    b.shutdown_all();
    0
}
/// 物理内存总量（MB）。取不到就返回 0，界面会退回 schema 里的上限。
fn mem_total_mb() -> u64 {
    let mut st: windows_sys::Win32::System::SystemInformation::MEMORYSTATUSEX =
        unsafe { std::mem::zeroed() };
    st.dwLength = std::mem::size_of::<windows_sys::Win32::System::SystemInformation::MEMORYSTATUSEX>()
        as u32;
    if unsafe { windows_sys::Win32::System::SystemInformation::GlobalMemoryStatusEx(&mut st) } == 0 {
        return 0;
    }
    st.ullTotalPhys / (1024 * 1024)
}

/// 当前可用物理内存（MB）。
fn mem_avail_mb() -> u64 {
    let mut st: windows_sys::Win32::System::SystemInformation::MEMORYSTATUSEX =
        unsafe { std::mem::zeroed() };
    st.dwLength = std::mem::size_of::<windows_sys::Win32::System::SystemInformation::MEMORYSTATUSEX>()
        as u32;
    if unsafe { windows_sys::Win32::System::SystemInformation::GlobalMemoryStatusEx(&mut st) } == 0 {
        return 0;
    }
    st.ullAvailPhys / (1024 * 1024)
}

fn register_host_services() {    let b = bus::get();

    b.register_host_service("miy.app.ping", |_| br#"{"pong":true}"#.to_vec());

    // 启动动画播完了，该把界面换上来。页面只报一声，窗口怎么切是宿主的事。
    b.register_host_service("miy.app.intro", |req| {
        let v = serde_json::from_slice::<serde_json::Value>(req).unwrap_or_default();
        if v.get("action").and_then(|x| x.as_str()) == Some("done") {
            ui::intro_done();
        }
        br#"{"ok":true}"#.to_vec()
    });

    b.register_host_service("miy.app.info", |_| {
        // 「与启动器同尺寸」要按客户区（不含标题栏）开游戏，所以尺寸得一起报出去。
        let (win_w, win_h) = ui::client_size();
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "abi": miy_abi::MIY_ABI_VERSION,
            "revision": miy_abi::MIY_ABI_REVISION,
            "dataRoot": paths::data_root().display().to_string(),
            "pluginsDir": paths::plugins_dir().display().to_string(),
            // 插件要按"启动器同级的 versions 目录"作为默认游戏根，得知道 exe 在哪。
            "exeDir": paths::exe_dir().display().to_string(),
            "log": log::path().map(|p| p.display().to_string()),
            "window": { "width": win_w, "height": win_h },
            // 内存滑杆要按本机物理内存封顶，和 PCL2 一样不让用户拖到不存在的容量。
            "memory": {
                "total": mem_total_mb(),
                "avail": mem_avail_mb(),
            },
        })
        .to_string()
        .into_bytes()
    });

    b.register_host_service("miy.app.plugins", |_| {
        let list: Vec<serde_json::Value> = bus::get()
            .snapshot()
            .iter()
            .map(|p| {
                json!({
                    "id": p.id,
                    "version": p.version,
                    "started": p.started,
                    "faulted": p.faulted,
                    "calls": p.calls,
                    "faults": p.faults,
                    "timeouts": p.timeouts,
                    // 内置插件的 dir 是空的，界面上别显示成一个空路径。
                    "dir": if p.dir.is_empty() {
                        "启动器内置".to_string()
                    } else {
                        p.dir.clone()
                    },
                })
            })
            .collect();
        json!({
            "root": paths::plugins_dir().display().to_string(),
            "plugins": list,
        })
        .to_string()
        .into_bytes()
    });

    // 插件贡献的界面页面：宿主只给槽位，页面怎么排是插件的事。
    b.register_host_service("miy.app.pages", |_| {
        let mut pages: Vec<serde_json::Value> = bus::get()
            .snapshot()
            .iter()
            .flat_map(|p| p.pages.iter().map(move |g| (p.started, g.clone())))
            .filter(|(started, _)| *started)
            .map(|(_, g)| {
                json!({
                    "plugin": g.plugin,
                    "id": g.id,
                    "title": g.title,
                    "slot": g.slot,
                    "order": g.order,
                })
            })
            .collect();
        // 导航顺序由页面自己的 `order` 决定，宿主不替插件排序。
        pages.sort_by_key(|p| p["order"].as_i64().unwrap_or(0));
        json!({ "pages": pages }).to_string().into_bytes()
    });

    // 页面正文：读 `plugins/<id>/ui/<entry>`。路径必须挡掉 `..`、盘符和
    // 绝对路径 —— 插件的目录边界就是它的权限边界。
    b.register_host_service("miy.app.page", |req| {
        let v = serde_json::from_slice::<serde_json::Value>(req).unwrap_or_default();
        let plugin = v.get("plugin").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let Some(page) = bus::get()
            .snapshot()
            .iter()
            .flat_map(|p| p.pages.iter())
            .find(|g| g.plugin == plugin && g.id == id)
            .cloned()
        else {
            return json!({ "ok": false, "error": "页面不存在" })
                .to_string()
                .into_bytes();
        };
        let bad = |s: &str| s.contains("..") || s.contains(':') || s.starts_with('/') || s.starts_with('\\');
        if bad(&page.entry) {
            return json!({ "ok": false, "error": "页面路径无效" })
                .to_string()
                .into_bytes();
        }
        // 内置插件（编进 exe 的 miy.mc）不走磁盘。
        if let Some(html) = embedded::page(&plugin, &page.entry) {
            return json!({
                "ok": true,
                "plugin": plugin,
                "id": page.id,
                "title": page.title,
                "html": html,
            })
            .to_string()
            .into_bytes();
        }
        let file = paths::plugins_dir()
            .join(&plugin)
            .join("ui")
            .join(&page.entry);
        match std::fs::read_to_string(&file) {
            Ok(html) => json!({
                "ok": true,
                "plugin": plugin,
                "id": page.id,
                "title": page.title,
                "html": html,
            })
            .to_string()
            .into_bytes(),
            Err(e) => {
                log::warn(&format!("读取页面失败 {}: {e}", file.display()));
                json!({ "ok": false, "error": "页面文件缺失，请重新安装启动器。" })
                    .to_string()
                    .into_bytes()
            }
        }
    });

    // 页面拿不到控制台，日志得能从界面上看。
    b.register_host_service("miy.app.log", |_| {        let text = log::path()            .and_then(|p| std::fs::read(p).ok())
            .map(|bytes| {
                let start = bytes.len().saturating_sub(24 * 1024);
                String::from_utf8_lossy(&bytes[start..]).into_owned()
            })
            .unwrap_or_default();
        json!({ "text": text }).to_string().into_bytes()
    });

    // 网络。插件不会自带 TLS —— 那会把每个插件的体积翻几倍，也违背
    // "系统有什么就用什么"。所以 HTTP 只有这一组服务。
    b.register_host_service("miy.net.get", |req| {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(req) else {
            return json!({ "ok": false, "error": "请求不是合法 JSON" })
                .to_string()
                .into_bytes();
        };
        let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("");
        if url.is_empty() {
            return json!({ "ok": false, "error": "缺少 url" }).to_string().into_bytes();
        }
        let timeout = Duration::from_millis(
            v.get("timeout_ms").and_then(|x| x.as_u64()).unwrap_or(30_000).clamp(1_000, 600_000),
        );
        let max = v
            .get("max_bytes")
            .and_then(|x| x.as_u64())
            .unwrap_or(8 * 1024 * 1024)
            .clamp(1024, 512 * 1024 * 1024) as usize;
        // 可选请求头：插件需要额外头（如 Accept）时走这里，凭据不写死在插件里。
        let hdrs: Vec<(String, String)> = v
            .get("headers")
            .and_then(|h| h.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let hdr_refs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        match net::send("GET", url, &hdr_refs, None, timeout, max) {
            Ok(r) if r.status == 200 => json!({
                "ok": true,
                "status": r.status,
                "text": String::from_utf8_lossy(&r.body),
                "bytes": r.body.len(),
            })
            .to_string()
            .into_bytes(),
            Ok(r) => json!({ "ok": false, "status": r.status, "error": format!("HTTP {}", r.status) })
                .to_string()
                .into_bytes(),
            Err(e) => {
                log::warn(&format!("GET {url} 失败：{e}"));
                json!({ "ok": false, "error": e }).to_string().into_bytes()
            }
        }
    });

    b.register_host_service("miy.net.download", |req| {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(req) else {
            return json!({ "ok": false, "error": "请求不是合法 JSON" })
                .to_string()
                .into_bytes();
        };
        let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("");
        let path = v.get("path").and_then(|x| x.as_str()).unwrap_or("");
        if url.is_empty() || path.is_empty() {
            return json!({ "ok": false, "error": "缺少 url 或 path" })
                .to_string()
                .into_bytes();
        }
        let timeout = Duration::from_millis(
            v.get("timeout_ms").and_then(|x| x.as_u64()).unwrap_or(120_000).clamp(1_000, 900_000),
        );
        // 期望大小：给了就校验。镜像节点偶尔会回 200 + 半截内容（实测某个节点固定截在 8 MiB），
        // 不校验的话会写下一个"看起来成功"的坏文件，最后在 sha1 那一步才炸。
        let expect = v.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
        let started = std::time::Instant::now();
        match net::download(url, std::path::Path::new(path), timeout) {
            Ok(n) if expect > 0 && n != expect => {
                let _ = std::fs::remove_file(path);
                json!({
                    "ok": false,
                    "error": format!("服务器返回了不完整的文件（{n} / {expect} 字节）")
                })
                .to_string()
                .into_bytes()
            }
            Ok(n) => {
                bus::get().emit(
                    "net.downloaded",
                    json!({ "path": path, "bytes": n, "ms": started.elapsed().as_millis() as u64 })
                        .to_string()
                        .as_bytes(),
                );
                json!({ "ok": true, "bytes": n }).to_string().into_bytes()
            }
            Err(e) => {
                log::warn(&format!("下载 {url} 失败：{e}"));
                json!({ "ok": false, "error": e }).to_string().into_bytes()
            }
        }
    });

    // 分段下载：大文件按字节区间并行取，写到同一文件的各自偏移处。
    b.register_host_service("miy.net.download_range", |req| {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(req) else {
            return json!({ "ok": false, "error": "请求不是合法 JSON" })
                .to_string()
                .into_bytes();
        };
        let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("");
        let path = v.get("path").and_then(|x| x.as_str()).unwrap_or("");
        let offset = v.get("offset").and_then(|x| x.as_u64()).unwrap_or(0);
        let length = v.get("length").and_then(|x| x.as_u64()).unwrap_or(0);
        if url.is_empty() || path.is_empty() || length == 0 {
            return json!({ "ok": false, "error": "缺少 url / path / length" })
                .to_string()
                .into_bytes();
        }
        let timeout = Duration::from_millis(
            v.get("timeout_ms").and_then(|x| x.as_u64()).unwrap_or(120_000).clamp(1_000, 900_000),
        );
        match net::download_range(url, std::path::Path::new(path), offset, length, timeout) {
            Ok(n) => {
                let (s, l) = net::take_rate();
                json!({ "ok": true, "bytes": n, "rl_short": s, "rl_long": l })
                    .to_string()
                    .into_bytes()
            }
            Err(e) => {
                log::debug(&format!(
                    "分段下载 {url} [{offset}+{length}] 失败（{} {}）：{}",
                    e.kind, e.status, e.msg
                ));
                let (s, l) = net::take_rate();
                json!({
                    "ok": false,
                    "error": e.msg,
                    "kind": e.kind,
                    "status": e.status,
                    "retry_after": e.retry_after,
                    "rl_short": s,
                    "rl_long": l,
                })
                .to_string()
                .into_bytes()
            }
        }
    });
    // POST。登录链路上每个端点都是 POST：设备码、令牌交换、XBL、XSTS、MC。
    // `ct` 只有两种：form（OAuth）和 json（剩下全是对）。
    b.register_host_service("miy.net.post", |req| {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(req) else {
            return json!({ "ok": false, "error": "请求不是合法 JSON" })
                .to_string()
                .into_bytes();
        };
        let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("");
        let body = v.get("body").and_then(|x| x.as_str()).unwrap_or("");
        if url.is_empty() {
            return json!({ "ok": false, "error": "缺少 url" }).to_string().into_bytes();
        }
        let ct = v.get("ct").and_then(|x| x.as_str()).unwrap_or("json");
        let timeout = Duration::from_millis(
            v.get("timeout_ms").and_then(|x| x.as_u64()).unwrap_or(30_000).clamp(1_000, 600_000),
        );
        let r = if ct == "form" {
            net::post_form(url, body, timeout, 4 * 1024 * 1024)
        } else {
            net::post_json(url, body, timeout, 4 * 1024 * 1024)
        };
        match r {
            Ok(r) => json!({
                "ok": r.status == 200,
                "status": r.status,
                "text": String::from_utf8_lossy(&r.body),
                "bytes": r.body.len(),
            })
            .to_string()
            .into_bytes(),
            Err(e) => {
                log::warn(&format!("POST {url} 失败：{e}"));
                json!({ "ok": false, "error": e }).to_string().into_bytes()
            }
        }
    });

    // 账号（只走设备码：不需要密钥、不需要重定向 URI，浏览器只用来输码）。
    // `miy.accounts.token` 只给插件用 —— 页面的白名单里没有它。
    b.register_host_service("miy.accounts.list", |_| accounts::list().to_string().into_bytes());
    b.register_host_service("miy.accounts.skin", |req| accounts::skin(req).to_string().into_bytes());

    // 自检不发真登录，只验「客户端 ID / 登录端点 / 公共客户端流」
    // 这三件事配得对不对 —— 页面在开浏览器之前先调它，能省掉一轮无谓跳转。
    b.register_host_service("miy.accounts.check", |_| accounts::check().to_string().into_bytes());

    b.register_host_service("miy.accounts.start", |_| match accounts::start() {
        Ok(v) => v.to_string().into_bytes(),
        Err(e) => {
            log::warn(&format!("登录启动失败：{e}"));
            json!({ "ok": false, "error": e }).to_string().into_bytes()
        }
    });

    b.register_host_service("miy.accounts.poll", |_| match accounts::poll() {
        Ok(v) => {
            if v.get("status").and_then(|x| x.as_str()) == Some("ok") {
                let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("");
                bus::get().emit("accounts.changed", json!({ "name": name }).to_string().as_bytes());
            }
            v.to_string().into_bytes()
        }
        Err(e) => json!({ "status": "error", "error": e }).to_string().into_bytes(),
    });

    b.register_host_service("miy.accounts.cancel", |_| {
        accounts::cancel();
        json!({ "ok": true }).to_string().into_bytes()
    });

    b.register_host_service("miy.accounts.logout", |_| {
        accounts::logout();
        bus::get().emit("accounts.changed", json!({ "name": "" }).to_string().as_bytes());
        json!({ "ok": true }).to_string().into_bytes()
    });

    // 启动游戏时才拿它。**不要在别处暴露**：这是能直接冒充玩家的东西。
    b.register_host_service("miy.accounts.token", |_| match accounts::token() {
        Ok(v) => v.to_string().into_bytes(),
        Err(e) => json!({ "ok": false, "error": e }).to_string().into_bytes(),
    });

    // 启动时统一取身份：登录了就正版，否则用用户配的离线身份。
    // 它带 token，所以**不在页面白名单里** —— 只有插件能调。
    b.register_host_service("miy.accounts.identity", |_| {
        accounts::identity().to_string().into_bytes()
    });

    // 离线身份：没登录也能改名字和 UUID（PCL2 也允许这么干）。
    b.register_host_service("miy.accounts.offline", |_| {
        accounts::offline_get().to_string().into_bytes()
    });

    b.register_host_service("miy.accounts.switch", |req| {
        let out = accounts::switch(req);
        if out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            bus::get().emit("accounts.changed", b"{}");
        }
        out.to_string().into_bytes()
    });
    b.register_host_service("miy.accounts.remove", |req| {
        let out = accounts::remove(req);
        if out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            bus::get().emit("accounts.changed", b"{}");
        }
        out.to_string().into_bytes()
    });

    b.register_host_service("miy.accounts.offline.set", |req| {
        let out = accounts::offline_set(req);
        if out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let name = out.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            bus::get().emit("accounts.changed", json!({ "name": name }).to_string().as_bytes());
        }
        out.to_string().into_bytes()
    });

    // 设置页只跟这三个服务打交道：读快照、改一项、恢复默认。
    // 校验和落盘都在宿主里做，页面不碰文件，也不碰别的插件的键。
    b.register_host_service("miy.app.settings", |_| {
        settings::snapshot().to_string().into_bytes()
    });

    b.register_host_service("miy.app.setting.set", |req| {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(req) else {
            return json!({ "ok": false, "error": "请求不是合法 JSON" })
                .to_string()
                .into_bytes();
        };
        let key = v.get("key").and_then(|k| k.as_str()).unwrap_or("").to_string();
        let value = v.get("value").cloned().unwrap_or(serde_json::Value::Null);
        match settings::set(&key, value) {
            Ok(saved) => json!({ "ok": true, "key": key, "value": saved })
                .to_string()
                .into_bytes(),
            Err(e) => json!({ "ok": false, "key": key, "error": e })
                .to_string()
                .into_bytes(),
        }
    });

    b.register_host_service("miy.app.setting.reset", |req| {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(req) else {
            return json!({ "ok": false, "error": "请求不是合法 JSON" })
                .to_string()
                .into_bytes();
        };
        let key = v.get("key").and_then(|k| k.as_str()).unwrap_or("").to_string();
        match settings::reset(&key) {
            Ok(default) => json!({ "ok": true, "key": key, "value": default })
                .to_string()
                .into_bytes(),
            Err(e) => json!({ "ok": false, "key": key, "error": e })
                .to_string()
                .into_bytes(),
        }
    });

    // 窗口按钮。页面只发动作名，真正的 ShowWindow 回 UI 线程做 —— 服务可能
    // 跑在 IPC 线程上，直接操作窗口会跟消息循环抢锁。
    b.register_host_service("miy.app.window", |req| {
        let action = field_of(req, "action");
        let ok = ui::post_action(&action);
        json!({ "ok": ok, "action": action }).to_string().into_bytes()
    });

    // 打开目录：日志、数据、插件。用系统资源管理器，不自己写文件浏览器。
    b.register_host_service("miy.app.open", |req| {
        // 带 url 就是开网页（登录时把用户直接送到 microsoft.com/link）。
        let url = field_of(req, "url");
        if !url.is_empty() {
            return match std::process::Command::new("rundll32")
                .args(["url.dll,FileProtocolHandler", &url])
                .spawn()
            {
                Ok(_) => json!({ "ok": true, "url": url }).to_string().into_bytes(),
                Err(e) => json!({ "ok": false, "url": url, "error": e.to_string() })
                    .to_string()
                    .into_bytes(),
            };
        }
        let what = field_of(req, "what");
        // 直接给路径：版本详情里的「打开所在文件夹」。
        let direct = field_of(req, "path");
        if !direct.is_empty() {
            return match std::process::Command::new("explorer")
                .arg(std::path::PathBuf::from(&direct))
                .spawn()
            {
                Ok(_) => json!({ "ok": true, "path": direct }).to_string().into_bytes(),
                Err(e) => json!({ "ok": false, "path": direct, "error": e.to_string() })
                    .to_string()
                    .into_bytes(),
            };
        }
        let dir = match what.as_str() {
            "logs" => paths::logs_dir(),
            "plugins" => paths::plugins_dir(),
            // 游戏目录现在默认是启动器旁边的 versions 目录（mp 插件可在设置里改）。
            "game" => match settings::effective_str("miy.mc.root") {
                Some(custom) if !custom.is_empty() => std::path::PathBuf::from(custom),
                _ => paths::exe_dir().join("MC"),
            },
            _ => paths::data_root(),
        };
        match std::process::Command::new("explorer").arg(&dir).spawn() {
            Ok(_) => json!({ "ok": true, "path": dir.display().to_string() })
                .to_string()
                .into_bytes(),
            Err(e) => json!({
                "ok": false,
                "path": dir.display().to_string(),
                "error": e.to_string(),
            })
            .to_string()
            .into_bytes(),
        }
    });

    // 页面上点「浏览…」时弹出的就是系统自己的文件夹选择器：
    // 玩家看到的是他熟悉的 Windows 对话框，路径合法性也不用我们猜。
    b.register_host_service("miy.app.pickFile", |req| {
        let title = field_of(req, "title");
        let title = if title.is_empty() {
            "选择文件".to_string()
        } else {
            title
        };
        match pick_file(&title) {
            Ok(path) => json!({ "ok": true, "path": path }).to_string().into_bytes(),
            Err(e) if e == "cancelled" => json!({ "ok": false, "cancelled": true }).to_string().into_bytes(),
            Err(e) => json!({ "ok": false, "error": e }).to_string().into_bytes(),
        }
    });
    // 「另存为」：导出整合包时让玩家自己挑位置和文件名。
    // 和 pickFile 的区别是它允许输入一个还不存在的路径。
    b.register_host_service("miy.app.saveFile", |req| {
        let title = field_of(req, "title");
        let title = if title.is_empty() {
            "保存文件".to_string()
        } else {
            title
        };
        let name = field_of(req, "name");
        match save_file(&title, &name) {
            Ok(path) => json!({ "ok": true, "path": path }).to_string().into_bytes(),
            Err(e) if e == "cancelled" => json!({ "ok": false, "cancelled": true }).to_string().into_bytes(),
            Err(e) => json!({ "ok": false, "error": e }).to_string().into_bytes(),
        }
    });
    b.register_host_service("miy.app.pickFolder", |req| {        let title = field_of(req, "title");
        let title = if title.is_empty() {
            "选择文件夹".to_string()
        } else {
            title
        };
        match pick_folder(&title) {
            Ok(path) => json!({ "ok": true, "path": path }).to_string().into_bytes(),
            Err(e) => json!({ "ok": false, "error": e }).to_string().into_bytes(),
        }
    });

    // 页面是 sandbox iframe，`navigator.clipboard` 在那种文档里不可用
    // （安全上下文 + 用户激活都满足不了），所以复制得由宿主代劳。
    b.register_host_service("miy.app.clipboard", |req| {
        let text = field_of(req, "text");
        if text.is_empty() {
            return json!({ "ok": false, "error": "没有可复制的内容" })
                .to_string()
                .into_bytes();
        }
        match clipboard::set_text(&text) {
            Ok(()) => json!({ "ok": true, "chars": text.chars().count() })
                .to_string()
                .into_bytes(),
            Err(e) => json!({ "ok": false, "error": e }).to_string().into_bytes(),
        }
    });
}

/// 从请求 JSON 里取一个字符串字段；取不到就是空串（服务各自处理空的意思）。
fn field_of(req: &[u8], key: &str) -> String {
    serde_json::from_slice::<serde_json::Value>(req)
        .ok()
        .and_then(|v| v.get(key).and_then(|x| x.as_str()).map(str::to_string))
        .unwrap_or_default()
}

/// 系统文件夹选择器（`IFileOpenDialog` + `FOS_PICKFOLDERS`）。
///
/// 用原生对话框而不是自己画一个，是因为它自带网络位置、最近使用、新建文件夹、
/// 中文路径这些玩家已经习惯的东西。调用发生在 IPC 的工作线程上，所以这里自己
/// 初始化 COM（`COINIT_APARTMENTTHREADED`，Shell 的老规矩）。
fn pick_folder(title: &str) -> Result<String, String> {
    use windows::core::{HSTRING, PWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH, FOS_PICKFOLDERS,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        // S_FALSE 也是成功（这个线程已经初始化过）；失败就换一条路。
        let did_init = hr.is_ok() || hr == windows::Win32::Foundation::S_FALSE;
        if !hr.is_ok() && hr != windows::Win32::Foundation::S_FALSE {
            return Err(format!("COM 初始化失败：{hr:?}"));
        }
        let result = (|| -> Result<String, String> {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("创建对话框失败：{e}"))?;
            let options = dialog.GetOptions().map_err(|e| e.to_string())?;
            dialog
                .SetOptions(options | FOS_PICKFOLDERS)
                .map_err(|e| format!("设置选项失败：{e}"))?;
            dialog
                .SetTitle(&HSTRING::from(title))
                .map_err(|e| format!("设置标题失败：{e}"))?;
            let hwnd = GetForegroundWindow();
            if dialog.Show(Some(hwnd)).is_err() {
                // Show 返回失败基本就是用户点了取消（HRESULT 0x800704C7）。
                return Err("cancelled".to_string());
            }
            let item = dialog.GetResult().map_err(|e| format!("拿结果失败：{e}"))?;
            let pw: PWSTR = item
                .GetDisplayName(SIGDN_FILESYSPATH)
                .map_err(|e| format!("拿路径失败：{e}"))?;
            let path = pw.to_string().unwrap_or_default();
            windows::Win32::System::Com::CoTaskMemFree(Some(pw.0 as *const _));
            if path.is_empty() {
                return Err("cancelled".to_string());
            }
            Ok(path)
        })();
        if did_init {
            CoUninitialize();
        }
        result
    }
}

/// 系统文件选择器：只用来挑整合包（`.zip`）。
fn pick_file(title: &str) -> Result<String, String> {
    use windows::core::{HSTRING, PWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, IShellItem, SIGDN_FILESYSPATH,
    };
    use windows::Win32::Foundation::GetLastError;

    let did_init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok() };
    let result = (|| -> Result<String, String> {
        unsafe {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("创建对话框失败：{e}"))?;
            dialog
                .SetTitle(&HSTRING::from(title))
                .map_err(|e| format!("设置标题失败：{e}"))?;
            let hwnd = windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
            if dialog.Show(Some(hwnd)).is_err() {
                let _ = GetLastError();
                return Err("cancelled".to_string());
            }
            let item: IShellItem = dialog.GetResult().map_err(|e| format!("拿结果失败：{e}"))?;
            let pw: PWSTR = item
                .GetDisplayName(SIGDN_FILESYSPATH)
                .map_err(|e| format!("拿路径失败：{e}"))?;
            let path = pw.to_string().unwrap_or_default();
            CoTaskMemFree(Some(pw.0 as *const _));
            if path.is_empty() {
                return Err("cancelled".to_string());
            }
            Ok(path)
        }
    })();
    if did_init {
        unsafe { CoUninitialize() };
    }
    result
}

/// 系统「另存为」对话框（`IFileSaveDialog`）。
///
/// 和 [`pick_file`] 的区别：这里允许输入一个**还不存在**的路径，所以导出整合包
/// 时玩家能自己决定放哪、叫什么名字，而不是只能接受我们拼出来的文件名。
fn save_file(title: &str, name: &str) -> Result<String, String> {
    use windows::core::{HSTRING, PWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        FileSaveDialog, IFileSaveDialog, IShellItem, SIGDN_FILESYSPATH,
    };

    let did_init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok() };
    let result = (|| -> Result<String, String> {
        unsafe {
            let dialog: IFileSaveDialog =
                CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("创建对话框失败：{e}"))?;
            dialog
                .SetTitle(&HSTRING::from(title))
                .map_err(|e| format!("设置标题失败：{e}"))?;
            if !name.is_empty() {
                dialog
                    .SetFileName(&HSTRING::from(name))
                    .map_err(|e| format!("设置文件名失败：{e}"))?;
            }
            dialog
                .SetDefaultExtension(&HSTRING::from("zip"))
                .map_err(|e| format!("设置扩展名失败：{e}"))?;
            let hwnd = windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
            if dialog.Show(Some(hwnd)).is_err() {
                return Err("cancelled".to_string());
            }
            let item: IShellItem = dialog.GetResult().map_err(|e| format!("拿结果失败：{e}"))?;
            let pw: PWSTR = item
                .GetDisplayName(SIGDN_FILESYSPATH)
                .map_err(|e| format!("拿路径失败：{e}"))?;
            let path = pw.to_string().unwrap_or_default();
            CoTaskMemFree(Some(pw.0 as *const _));
            if path.is_empty() {
                return Err("cancelled".to_string());
            }
            Ok(path)
        }
    })();
    if did_init {
        unsafe { CoUninitialize() };
    }
    result
}

// ---- 单实例 ----------------------------------------------------------------
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 已经有一个 MIYLauncher 在跑时，把它的窗口叫出来并返回退出码。
fn single_instance_guard() -> Option<i32> {
    let name = wide("Local\\MIYLauncher.SingleInstance");
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if h.is_null() {
            log::warn("创建单实例互斥体失败，跳过检查");
            return None;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let class = wide("MIYLauncherWindow");
            let hwnd = FindWindowW(class.as_ptr(), std::ptr::null());
            if !hwnd.is_null() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                let _ = SetForegroundWindow(hwnd);
                log::info("已有实例在运行，已切到前台");
            } else {
                log::info("已有实例在运行（窗口尚未创建）");
            }
            return Some(0);
        }
        let _ = INSTANCE_HANDLE.set(h as usize);
        None
    }
}

// ---- 插件加载 --------------------------------------------------------------

/// 插件加载必须在后台线程：spike 实测冷启动 242 ms、热 8.7 ms，
/// 这点时间足够让首帧掉到 300 ms 之外。
/// 把编译进 exe 的插件装上（见 [`embedded`]）。
///
/// 失败只记日志、不退出：内置插件自己有 bug 时，宿主至少要能把窗口开出来、
/// 把错误显示给用户，而不是双击之后什么都没有。
fn load_builtin_plugins() -> Vec<Result<String, String>> {
    let t = Instant::now();
    let outcome = bus::load_embedded(embedded::MC_MANIFEST, miy_mc::miy_plugin_entry_v1);
    let r = match outcome {
        Ok(h) => {
            let summary = format!(
                "{} v{} 内嵌加载，{} µs",
                h.id,
                h.version,
                t.elapsed().as_micros()
            );
            log::info(&format!("[plugin] {summary}"));
            bus::get().insert(std::sync::Arc::new(h));
            bus::get().emit("plugin.loaded", summary.as_bytes());
            Ok(summary)
        }
        Err(e) => {
            let msg = format!("内嵌插件 miy.mc 加载失败：{e}");
            log::error(&format!("[plugin] {msg}"));
            bus::get().emit("plugin.failed", msg.as_bytes());
            Err(msg)
        }
    };
    vec![r]
}

fn spawn_plugin_loader() {
    let spawned = std::thread::Builder::new()
        .name("plugin-loader".into())
        .spawn(|| {
            let t = Instant::now();
            let root = paths::plugins_dir();
            let results = bus::load_all(&root);
            // 统计口径跟着注册表走：内置插件不是从磁盘来的，但用户看到的
            // "插件加载完成"必须把 miy.mc 算进去，否则界面会显示 0 个插件。
            let mut first_error = None;
            for r in &results {
                if let Err(e) = r {
                    log::warn(&format!("插件加载失败: {e}"));
                    if first_error.is_none() {
                        first_error = Some(e.clone());
                    }
                }
            }
            let snapshot = bus::get().snapshot();
            let ok = snapshot.iter().filter(|p| p.started).count();
            let total = snapshot.len();
            log::info(&format!(
                "插件加载完成 {ok}/{total} 个（磁盘 {} 个），用时 {} ms",
                results.len(),
                t.elapsed().as_millis()
            ));
            // 加载失败必须让用户看见，不能只躺在日志里。
            if let Some(e) = first_error {
                let more = results.iter().filter(|r| r.is_err()).count();
                let msg = if more > 1 {
                    format!("有 {more} 个插件未能加载，首个错误：{e}")
                } else {
                    format!("插件加载失败：{e}")
                };
                bus::get().emit("host.error", msg.as_bytes());
            }
            bus::get().emit(
                "plugins.loaded",
                json!({ "ok": ok, "total": total }).to_string().as_bytes(),
            );
        });
    if let Err(e) = spawned {
        log::error(&format!("启动插件加载线程失败: {e}"));
    }
}


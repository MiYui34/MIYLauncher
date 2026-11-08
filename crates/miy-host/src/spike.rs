//! `--spike` 自检：不建窗口，把 ABI 的每条承诺当场验一遍。
//!
//! 验收项（docs/architecture.md §16）：
//! 1. 体积
//! 2. 插件发现 / ABI 校验 / start
//! 3. `call` 正向链路
//! 4. panic 隔离
//! 5. 访问冲突隔离（宿主存活）
//! 6. 重入被拒

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use miy_abi::*;

use crate::bus;

pub fn run() -> i32 {
    let t0 = Instant::now();
    println!("[spike] MIYLauncher {}", env!("CARGO_PKG_VERSION"));

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            println!("[spike] 拿不到自身路径: {e}");
            return 1;
        }
    };
    let size = fs::metadata(&exe).map(|m| m.len()).unwrap_or(0);
    println!("[spike] exe    : {}", exe.display());
    println!(
        "[spike] size   : {} bytes ({:.1} KiB)",
        size,
        size as f64 / 1024.0
    );

    let root = plugins_root(&exe);
    println!("[spike] plugins: {}", root.display());

    let mut failures = 0;

    // ---- 发现 + 加载 ------------------------------------------------------
    let candidates = bus::discover(&root);
    println!("[spike] 发现 {} 个插件目录", candidates.len());
    for c in &candidates {
        let t = Instant::now();
        match bus::load_one(c) {
            Ok(p) => {
                println!(
                    "[spike] OK  {:<10} v{:<7} start {} µs  dll={}",
                    p.id,
                    p.version,
                    t.elapsed().as_micros(),
                    p.dll.file_name().unwrap_or_default().to_string_lossy()
                );
                bus::get().insert(std::sync::Arc::new(p));
            }
            Err(e) => {
                println!("[spike] BAD {}: {e}", c.dir.display());
                failures += 1;
            }
        }
    }
    println!("[spike] 加载耗时 {} µs", t0.elapsed().as_micros());

    // ---- 正向：call 链路 --------------------------------------------------
    failures += expect_json("miy.hello.ping", b"", "ping", "pong", true);
    failures += expect_json("miy.hello.echo", b"miy-launcher", "echo", "miy-launcher", false);
    failures += expect_json("miy.hello.reenter", b"", "重入被拒", "reentrant", false);

    // ---- 负向：panic 隔离 -------------------------------------------------
    let t = Instant::now();
    match bus::get().call_default("miy.boom.panic", b"") {
        Err(MIY_ERR_PANIC) => {
            println!(
                "[spike] OK  插件 panic 被挡在边界内（{} µs），宿主存活",
                t.elapsed().as_micros()
            );
        }
        other => {
            println!("[spike] BAD panic 隔离失败: {other:?}");
            failures += 1;
        }
    }

    // ---- 负向：访问冲突隔离 -----------------------------------------------
    let t = Instant::now();
    match bus::get().call_default("miy.boom.crash", b"") {
        Err(MIY_ERR_FAULT) => {
            println!(
                "[spike] OK  插件访问冲突被 SEH 捕获（{} µs），宿主存活",
                t.elapsed().as_micros()
            );
        }
        other => {
            println!("[spike] BAD 访问冲突隔离失败: {other:?}");
            failures += 1;
        }
    }

    // ---- 崩过一个插件之后，别的插件还得好使 --------------------------------
    failures += expect_json("miy.hello.ping", b"", "崩溃后复发 ping", "pong", true);

    if is_faulted("miy.boom") {
        println!("[spike] OK  崩溃插件已被熔断（faulted=true，不再接客）");
    } else {
        println!("[spike] BAD 崩溃插件没有被熔断");
        failures += 1;
    }

    // ---- 超时：只放弃等待，不杀线程；插件之后还能接客 ------------------
    let t = Instant::now();
    match bus::get().call("miy.hello.slow", br#"{"ms":900}"#, Duration::from_millis(200)) {
        Err(MIY_ERR_TIMEOUT) => println!(
            "[spike] OK  超时被挡回（{} µs），没有 TerminateThread",
            t.elapsed().as_micros()
        ),
        other => {
            println!("[spike] BAD 超时未生效: {other:?}");
            failures += 1;
        }
    }
    // 慢调用还在跑，这一发会先等 gate（约 700 ms）再照常应答 ——
    // 证明超时只是"放弃等待"，插件本身没被弄坏。
    failures += expect_json("miy.hello.ping", b"", "超时后复发 ping", "pong", true);

    // ---- 事件通道 --------------------------------------------------------
    let _ = bus::get().call_default("miy.hello.emit", b"");
    let events = bus::get().drain_events(16);
    if events.iter().any(|(topic, _)| topic == "miy.hello.tick") {
        println!("[spike] OK  插件 emit 的事件已进宿主队列");
    } else {
        println!("[spike] BAD 事件没有到达宿主: {events:?}");
        failures += 1;
    }

    // ---- 设置系统 --------------------------------------------------------
    failures += settings_tests();

    // ---- task 原语 -------------------------------------------------------
    failures += task_tests();

    println!(
        "[spike] 统计: ping×3 调用计数 = {}",
        calls_of("miy.hello")
    );
    println!("[spike] 总耗时 {} µs", t0.elapsed().as_micros());

    // ---- stop 竞态：正在接客时停机 ---------------------------------------
    failures += shutdown_race_test();

    bus::get().shutdown_all();

    if failures == 0 {
        println!("[spike] 结论: PASS");
        0
    } else {
        println!("[spike] 结论: FAIL ({failures} 项)");
        1
    }
}

fn plugins_root(exe: &PathBuf) -> PathBuf {
    // 先看 exe 旁边的 plugins/
    let beside = exe.parent().unwrap_or(std::path::Path::new(".")).join("plugins");
    if beside.is_dir() {
        return beside;
    }
    // 再从当前工作目录找（cargo run 场景）
    let cwd = std::env::current_dir().unwrap_or_default().join("plugins");
    if cwd.is_dir() {
        return cwd;
    }
    beside
}

fn is_faulted(id: &str) -> bool {    bus::get()
        .snapshot()
        .iter()
        .any(|p| p.id == id && p.faulted)
}

fn calls_of(id: &str) -> u64 {
    bus::get()
        .snapshot()
        .iter()
        .find(|p| p.id == id)
        .map_or(0, |p| p.calls)
}

// ---- 设置系统 --------------------------------------------------------------

/// 验的是这条链路：插件交 schema → 宿主合并 → 校验 → 落盘 → 生效值。
fn settings_tests() -> i32 {
    let mut failures = 0;

    let snap = crate::settings::snapshot();
    let mut keys: Vec<String> = snap["schema"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| f["key"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    keys.sort();

    let mut missing = Vec::new();
    for need in [
        "core.log.level",
        "miy.hello.greeting",
        "miy.hello.mode",
        "miy.hello.retries",
    ] {
        if !keys.iter().any(|k| k == need) {
            missing.push(need);
        }
    }
    if missing.is_empty() {
        println!(
            "[spike] OK  设置 schema 合并了 {} 项（宿主 {} + 插件）",
            keys.len(),
            keys.iter().filter(|k| k.starts_with("core.")).count()
        );
    } else {
        println!("[spike] BAD 设置 schema 缺少 {missing:?}，实得 {keys:?}");
        failures += 1;
    }

    // 合法值：写进去、生效值跟着变
    match crate::settings::set("miy.hello.retries", serde_json::json!(7)) {
        Ok(v) => println!("[spike] OK  设置写入 miy.hello.retries = {v}"),
        Err(e) => {
            println!("[spike] BAD 设置写入失败: {e}");
            failures += 1;
        }
    }
    let now = crate::settings::snapshot();
    if now["values"]["miy.hello.retries"] == serde_json::json!(7) {
        println!("[spike] OK  设置生效值已更新（default 被覆盖，没有另存一份）");
    } else {
        println!(
            "[spike] BAD 设置生效值没更新: {}",
            now["values"]["miy.hello.retries"]
        );
        failures += 1;
    }

    // 越界 / 非法枚举：必须当场拒绝，不能存进去再说
    for (key, bad, label) in [
        ("miy.hello.retries", serde_json::json!(99), "越界整数"),
        ("miy.hello.mode", serde_json::json!("turbo"), "非法枚举"),
    ] {
        match crate::settings::set(key, bad) {
            Err(e) => println!("[spike] OK  {label}被拒: {e}"),
            Ok(v) => {
                println!("[spike] BAD {label}没被拒，写进去了 {v}");
                failures += 1;
            }
        }
    }

    failures += unknown_key_survives();

    match crate::settings::reset("miy.hello.retries") {
        Ok(v) => println!("[spike] OK  恢复默认 miy.hello.retries = {v}"),
        Err(e) => {
            println!("[spike] BAD 恢复默认失败: {e}");
            failures += 1;
        }
    }
    let _ = crate::settings::reset("miy.hello.greeting");
    failures
}

/// 插件来去自由，它留下的未知字段不能被宿主擦掉 —— 否则插件降级 = 配置丢失。
fn unknown_key_survives() -> i32 {
    let path = crate::paths::config_file();
    let mut doc: serde_json::Value = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(|v: &serde_json::Value| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    doc["__spike_marker"] = serde_json::json!("keep-me");

    let Ok(text) = serde_json::to_vec_pretty(&doc) else {
        println!("[spike] BAD 序列化 settings.json 失败");
        return 1;
    };
    if fs::write(&path, &text).is_err() {
        println!("[spike] BAD 写不了 settings.json（{}）", path.display());
        return 1;
    }
    crate::settings::reload();

    if let Err(e) = crate::settings::set("miy.hello.greeting", serde_json::json!("spike")) {
        println!("[spike] BAD 带未知字段时设置写入失败: {e}");
        return 1;
    }

    let after = fs::read_to_string(&path).unwrap_or_default();
    let survived = after.contains("keep-me");

    // 收拾干净：删掉标记、把问候语还原，别把自检的痕迹留给用户。
    let mut doc: serde_json::Value = serde_json::from_str(&after).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(o) = doc.as_object_mut() {
        o.remove("__spike_marker");
    }
    if let Ok(t) = serde_json::to_vec_pretty(&doc) {
        let _ = fs::write(&path, t);
    }
    crate::settings::reload();
    let _ = crate::settings::reset("miy.hello.greeting");

    if survived {
        println!("[spike] OK  未知字段在写盘后原样保留");
        0
    } else {
        println!("[spike] BAD 未知字段被写没了");
        1
    }
}

// ---- task 原语 -------------------------------------------------------------

/// 验的是 `spawn` → 任务体干活 → `cancel` → 任务体自己退出，全程没有强杀。
fn task_tests() -> i32 {
    let mut failures = 0;

    let resp = match bus::get().call_default("miy.hello.task", br#"{"ms":60}"#) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => {
            println!("[spike] BAD 起任务失败: {e}");
            return 1;
        }
    };
    let id = json_u64(&resp, "\"task\"");
    if id == 0 {
        println!("[spike] BAD 任务响应里没有 id: {resp}");
        return 1;
    }

    std::thread::sleep(Duration::from_millis(400));
    let ticks: usize = bus::get()
        .drain_events(128)
        .iter()
        .filter(|(t, _)| t == "miy.hello.task.tick")
        .count();
    if ticks >= 2 {
        println!("[spike] OK  任务 {id} 在跑（400 ms 内 {ticks} 次 tick）");
    } else {
        println!("[spike] BAD 任务没跑起来（tick={ticks}）");
        failures += 1;
    }

    if bus::get().cancel_task(id) {
        println!("[spike] OK  已请求取消任务 {id}");
    } else {
        println!("[spike] BAD 取消任务 {id} 失败：令牌已不在表里");
        failures += 1;
    }

    std::thread::sleep(Duration::from_millis(400));
    let after = bus::get().drain_events(128);
    // 取消是协作式的，插件在两次 tick 之间才看令牌，所以"取消后还多一次 tick"
    // 是合法行为。真正要证明的是：cancelled 事件之后就彻底安静了。
    let idx = after.iter().position(|(t, _)| t == "miy.hello.task.cancelled");
    let ticks_after = match idx {
        Some(i) => after[i + 1..]
            .iter()
            .filter(|(t, _)| t == "miy.hello.task.tick")
            .count(),
        None => usize::MAX,
    };
    std::thread::sleep(Duration::from_millis(200));
    let extra = bus::get()
        .drain_events(128)
        .iter()
        .filter(|(t, _)| t == "miy.hello.task.tick")
        .count();

    if idx.is_some() && ticks_after == 0 && extra == 0 {
        println!("[spike] OK  任务收到取消令牌后自己退出了（没有强杀线程，之后 0 次 tick）");
    } else {
        println!(
            "[spike] BAD 取消没生效：cancelled={} cancelled 之后 tick={ticks_after}，再等 200 ms 又 tick={extra}",
            idx.is_some()
        );
        failures += 1;
    }

    let live = bus::get().live_tasks();
    if live == 0 {
        println!("[spike] OK  任务表已清空");
    } else {
        println!("[spike] BAD 任务表没清空：剩 {live} 个");
        failures += 1;
    }
    failures
}

// ---- stop 竞态 -------------------------------------------------------------

/// 四个线程正在狂调插件，主线程此时 `shutdown_all`。
///
/// 这是最容易写成"偶发崩溃"的地方：闸门、`stop`、`FreeLibrary` 三者顺序错了，
/// 就会在有调用在飞的时候把 DLL 卸掉。要么全绿，要么当场炸，不给中间态。
fn shutdown_race_test() -> i32 {
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
    use std::sync::Arc;

    let stop = Arc::new(AtomicBool::new(false));
    let ok = Arc::new(AtomicU64::new(0));
    let err = Arc::new(AtomicU64::new(0));
    let first_err = Arc::new(AtomicI32::new(0));

    let mut workers = Vec::new();
    for _ in 0..4 {
        let stop = stop.clone();
        let ok = ok.clone();
        let err = err.clone();
        let first_err = first_err.clone();
        workers.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match bus::get().call(
                    "miy.hello.slow",
                    br#"{"ms":20}"#,
                    Duration::from_millis(500),
                ) {
                    Ok(_) => {
                        ok.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(code) => {
                        err.fetch_add(1, Ordering::Relaxed);
                        let _ = first_err.compare_exchange(
                            0,
                            code,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        );
                    }
                }
            }
        }));
    }

    // 让调用真的飞起来，再拔电源
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    bus::get().shutdown_all();
    let shutdown_us = t.elapsed().as_micros();
    stop.store(true, Ordering::Relaxed);
    for w in workers {
        let _ = w.join();
    }

    let (ok, err) = (ok.load(Ordering::Relaxed), err.load(Ordering::Relaxed));
    let code = first_err.load(Ordering::Relaxed);
    if ok + err == 0 {
        println!("[spike] BAD 停机竞态测试没跑到调用");
        1
    } else {
        // 停机之后的那一发会立刻拿到"没有这个服务"，不是挂住 —— 说明闸门没死锁。
        println!(
            "[spike] OK  停机与 4 个并发调用交错完成：shutdown {shutdown_us} µs，\
             成功 {ok} 次 / 快速拒绝 {err} 次（错误码 {code}），进程存活"
        );
        0
    }
}

fn json_u64(text: &str, key: &str) -> u64 {
    let Some(pos) = text.find(key) else {
        return 0;
    };
    let rest = &text[pos + key.len()..];
    let digits: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap_or(0)
}

fn expect_json(service: &str, req: &[u8], label: &str, needle: &str, exact_pong: bool) -> i32 {
    let t = Instant::now();
    match bus::get().call_default(service, req) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let hit = text.contains(needle) && (!exact_pong || text.contains("\"pong\":true"));
            if hit {
                println!(
                    "[spike] OK  {label}: {text}  ({} µs)",
                    t.elapsed().as_micros()
                );
                0
            } else {
                println!("[spike] BAD {label}: 期望含 {needle}，实得 {text}");
                1
            }
        }
        Err(code) => {
            println!("[spike] BAD {label}: 调用失败 err={code}");
            1
        }
    }
}

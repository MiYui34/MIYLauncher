//! miy.mc —— 第一个真实插件。
//!
//! 它把「版本清单 → 版本解析（含 inheritsFrom 合并）→ 运行库下载」这条链跑通，
//! 全部网络 I/O 都走宿主的 `miy.net.*`（WinHTTP + 系统 SChannel）。
//!
//! 这个插件同时是 ABI 的一次实战检验：它在 handler 内部调用宿主服务
//! （`miy.net.get` / `miy.net.download` / `miy.app.settings`），并且从自己的
//! 子线程里也调用宿主服务。两条路径都不该被"禁止重入"误伤 —— 宿主只拦
//! "插件调插件"。

use core::ffi::{c_char, c_void};
use miy_abi::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

mod crash;
mod dl;
mod java;
mod pack;

const ID: &[u8] = b"miy.mc\0";
const VERSION: &[u8] = b"0.1.0\0";

/// 下载源：全部走 BMCLAPI 镜像（用户要求不再回落到官方源）。
///
/// 镜像会把版本 JSON 原样转发，里面 `assetIndex.url` / `downloads.client.url` /
/// `libraries[].downloads.artifact.url` 仍写着 Mojang 域名，所以这里从不使用 JSON
/// 里的 url，而是按路径自己重拼。
/// 官方源。
const BMCLAPI: &str = "https://bmclapi2.bangbang93.com";
const MODRINTH: &str = "https://api.modrinth.com/v2";
const CURSEFORGE: &str = "https://api.curseforge.com/v1";
/// 国内镜像：MCIM 镜像 Modrinth / CurseForge 的接口，BMCLAPI 镜像 Mojang 的文件。
const MCIM: &str = "https://mod.mcimirror.top";
const MOJANG_ASSETS: &str = "https://resources.download.minecraft.net";
const MOJANG_LIBS: &str = "https://libraries.minecraft.net";
/// 自建中转站：只负责启动器更新、公告、配置，以及模组接口的最后兜底。
///
/// 大文件（客户端 jar、库、资源）**一律不经过它**：实测它的上行只有 1.36 MB/s，
/// 一次完整安装 742 MB 要十分钟，绕一圈反而比直连慢一个数量级。
/// 两个入口：直连 IP（8080，明文，0.2 s）与域名（Cloudflare 隧道，1.5–3.6 s）。
/// 用明文是因为 WinHTTP 不认自签证书（放开 SECURITY_FLAGS 仍回 0x80072F8F）。
const SITE_FAST: &str = "http://122.228.216.136:8080";
const SITE_TUNNEL: &str = "https://dash.miyui.vip";

/// 直连入口被判定不可用时的恢复时间点（冷却一分钟，避免每个请求都去撞墙）。
static SITE_DOWN_UNTIL: Mutex<Option<std::time::Instant>> = Mutex::new(None);
/// 两个入口都不通时的恢复时间点：这段时间直接走官方源。
static SITE_DEAD_UNTIL: Mutex<Option<std::time::Instant>> = Mutex::new(None);
const SITE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

fn site_ok() -> bool {
    match SITE_DOWN_UNTIL.lock() {
        Ok(g) => match *g {
            Some(t) => std::time::Instant::now() >= t,
            None => true,
        },
        Err(_) => true,
    }
}

fn site_fail() {
    if let Ok(mut g) = SITE_DOWN_UNTIL.lock() {
        *g = Some(std::time::Instant::now() + SITE_COOLDOWN);
    }
}

fn site_recover() {
    if let Ok(mut g) = SITE_DOWN_UNTIL.lock() {
        *g = None;
    }
}

/// 两个入口都不通：这段时间别再试中转站了。
fn site_dead() -> bool {
    match SITE_DEAD_UNTIL.lock() {
        Ok(g) => match *g {
            Some(t) => std::time::Instant::now() < t,
            None => false,
        },
        Err(_) => false,
    }
}

fn site_dead_mark() {
    if let Ok(mut g) = SITE_DEAD_UNTIL.lock() {
        *g = Some(std::time::Instant::now() + SITE_COOLDOWN);
    }
}

/// 当前该用哪个入口。
fn site_base() -> &'static str {
    if site_ok() {
        SITE_FAST
    } else {
        SITE_TUNNEL
    }
}

fn is_site(url: &str) -> bool {
    url.starts_with(SITE_FAST) || url.starts_with(SITE_TUNNEL)
}

/// 认出「模组接口」的地址，返回 (modrinth|curseforge, 剩余路径)。
fn mod_split(url: &str) -> Option<(&'static str, String)> {
    let mr = [
        format!("{MCIM}/modrinth/v2"),
        MODRINTH.to_string(),
        format!("{SITE_FAST}/modrinth/v2"),
        format!("{SITE_TUNNEL}/modrinth/v2"),
    ];
    let cf = [
        format!("{MCIM}/curseforge/v1"),
        CURSEFORGE.to_string(),
        format!("{SITE_FAST}/curseforge/v1"),
        format!("{SITE_TUNNEL}/curseforge/v1"),
    ];
    for p in &mr {
        if let Some(rest) = url.strip_prefix(p.as_str()) {
            return Some(("modrinth", rest.to_string()));
        }
    }
    for p in &cf {
        if let Some(rest) = url.strip_prefix(p.as_str()) {
            return Some(("curseforge", rest.to_string()));
        }
    }
    None
}

/// 把任意入口的地址换回官方地址。
fn direct_of(url: &str) -> String {
    if let Some((kind, rest)) = mod_split(url) {
        return if kind == "modrinth" {
            format!("{MODRINTH}{rest}")
        } else {
            format!("{CURSEFORGE}{rest}")
        };
    }
    if let Some(rest) = url
        .strip_prefix(SITE_FAST)
        .or_else(|| url.strip_prefix(SITE_TUNNEL))
    {
        if let Some(p) = rest.strip_prefix("/bmclapi") {
            return format!("{BMCLAPI}{p}");
        }
    }
    url.to_string()
}

/// BMCLAPI 的资源 / 库文件换回 Mojang 官方地址（BMCLAPI 挂了时的第二选择）。
fn mojang_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix(BMCLAPI)?;
    let rest = rest.split('?').next().unwrap_or(rest);
    if let Some(h) = rest.strip_prefix("/assets/") {
        if h.len() >= 2 {
            return Some(format!("{MOJANG_ASSETS}/{h}"));
        }
    }
    if let Some(p) = rest.strip_prefix("/libraries/") {
        return Some(format!("{MOJANG_LIBS}/{p}"));
    }
    None
}

/// 模组接口换到自建中转（最后兜底）。
fn site_of(url: &str) -> Option<String> {
    let (kind, rest) = mod_split(url)?;
    let base = site_base();
    Some(if kind == "modrinth" {
        format!("{base}/modrinth/v2{rest}")
    } else {
        format!("{base}/curseforge/v1{rest}")
    })
}

/// 模组接口换到 MCIM 镜像（不需要 API Key，国内直连比官方快）。
fn mcim_of(url: &str) -> Option<String> {
    let (kind, rest) = mod_split(url)?;
    Some(if kind == "modrinth" {
        format!("{MCIM}/modrinth/v2{rest}")
    } else {
        format!("{MCIM}/curseforge/v1{rest}")
    })
}

/// 一个请求的候选链：MCIM（仅模组接口）→ 主源 → 官方 → 自建中转（仅模组接口兜底）。
///
/// 原版资源（客户端 jar、库、资源）**不经过自建中转**：实测它的上行只有
/// 1.36 MB/s，绕一圈比直连慢一个数量级。
fn candidates(url: &str) -> Vec<String> {
    fn add(out: &mut Vec<String>, u: String) {
        if !u.is_empty() && !out.iter().any(|x| x == &u) {
            out.push(u);
        }
    }
    let mut out: Vec<String> = Vec::new();
    // 模组接口优先 MCIM：官方 API 在国内经常要好几秒，MCIM 通常 0.1–0.5 秒。
    if let Some(m) = mcim_of(url) {
        add(&mut out, m);
    }
    // 自建中转在冷却期里就别再撞它了，直接从官方源开始。
    if !(is_site(url) && site_dead()) {
        add(&mut out, url.to_string());
    }
    if let Some(m) = mojang_of(url) {
        add(&mut out, m);
    }
    add(&mut out, direct_of(url));
    if mod_split(url).is_some() && !site_dead() {
        if let Some(s) = site_of(url) {
            add(&mut out, s);
        }
    }
    out
}

fn mirror_base() -> String {
    BMCLAPI.to_string()
}

/// 不认 `Range` 的源（实测某个节点对 Range 请求直接回 200 整包）。
///
/// 记下来之后，分段下载就不再往它身上撞 —— 否则每个块都要先失败一次再换源，
/// 一个 40 MB 的文件能白烧十几秒。
static NO_RANGE: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn host_key(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    rest.split('/').next().unwrap_or(rest).to_string()
}

fn no_range(url: &str) -> bool {
    let key = host_key(url);
    NO_RANGE.lock().map(|v| v.iter().any(|h| h == &key)).unwrap_or(false)
}

pub(crate) fn mark_no_range(url: &str) {
    let key = host_key(url);
    if let Ok(mut v) = NO_RANGE.lock() {
        if !v.iter().any(|h| h == &key) {
            v.push(key);
        }
    }
}

/// 从第 `i` 个候选开始，挑第一个还愿意接 Range 的源。
///
/// 候选链在建 job 时就定好了，而"这个源不认 Range"是跑到一半才知道的 ——
/// 所以每次取源都要现查一遍，不能只靠建 job 时过滤。
pub(crate) fn pick_url(urls: &[String], i: usize) -> &str {
    let n = urls.len();
    if n == 0 {
        return "";
    }
    for k in 0..n {
        let u = &urls[(i + k) % n];
        if !no_range(u) {
            return u;
        }
    }
    &urls[i % n]
}

fn modrinth_base() -> String {
    format!("{MCIM}/modrinth/v2")
}

fn curseforge_base() -> String {
    // 按 MCIM 的接入指南走镜像：`api.curseforge.com/v1` → `mod.mcimirror.top/curseforge/v1`，
    // 镜像自己带 Key，启动器里不再存任何 CurseForge 凭据。
    format!("{MCIM}/curseforge/v1")
}

const MANIFEST_MIRROR: &str = "https://bmclapi2.bangbang93.com/mc/game/version_manifest_v2.json";
const MANIFEST_OFFICIAL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

/// 版本清单的取源顺序。
///
/// 清单只有 278 KB，但它是"能不能装"的总开关：镜像偶尔会滞后一两天（新版本
/// 出来了清单里还没有），这时候用户需要能自己切回官方。所以给它一个独立开关，
/// 而不是跟下载源绑在一起 —— 文件走镜像、清单走官方，是很常见的组合。
fn manifest_urls(h: &MiyHostV1) -> Vec<&'static str> {
    let prefer_official = call_json(h, "miy.app.settings", &json!({}))
        .ok()
        .and_then(|v| {
            v.get("values")
                .and_then(|x| x.get("miy.mc.manifest"))
                .and_then(Value::as_str)
                .map(|s| s == "官方" || s == "official")
        })
        .unwrap_or(false);
    if prefer_official {
        vec![MANIFEST_OFFICIAL, MANIFEST_MIRROR]
    } else {
        vec![MANIFEST_MIRROR, MANIFEST_OFFICIAL]
    }
}

// ---- 任务表 ---------------------------------------------------------------
//
// 下载是"已经在跑的事"，不该只属于某个页面的某次调用：用户切页、关页面、
// 甚至重新进来，都得能看见进度。所以进度先落在插件自己的静态表里，页面只是
// 一个观察者（`miy.mc.tasks` 拉全量，`mc.progress` 事件负责催它刷新）。
struct Task {
    id: u64,
    kind: &'static str,
    title: String,
    state: &'static str,
    done: u64,
    total: u64,
    bytes: u64,
    started: u64,
    error: String,
    /// 取消请求：下载线程每轮都看一眼，看到就收工。
    cancel: Arc<AtomicBool>,
    /// 这次任务真正新写出来的文件。取消时按这个清单回滚，
    /// 只删自己造的，别去动本来就在磁盘上的东西。
    created: Vec<String>,
}

static TASKS: Mutex<Vec<Task>> = Mutex::new(Vec::new());
static NEXT_TASK: AtomicU64 = AtomicU64::new(1);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 开一个任务，返回它的 id 和取消令牌。
///
/// 令牌交回调用方，由它传给下载循环 —— 任务表里也留一份，`miy.mc.task.cancel`
/// 才找得到该改哪个。
fn task_new(kind: &'static str, title: &str, total: u64) -> (u64, Arc<AtomicBool>) {
    let id = NEXT_TASK.fetch_add(1, Ordering::SeqCst);
    let cancel = Arc::new(AtomicBool::new(false));
    if let Ok(mut v) = TASKS.lock() {
        v.push(Task {
            id,
            kind,
            title: title.to_string(),
            state: "running",
            done: 0,
            total,
            bytes: 0,
            started: now_secs(),
            error: String::new(),
            cancel: Arc::clone(&cancel),
            created: Vec::new(),
        });
        // 只留最近 24 条，历史任务没人会翻第二遍。
        if v.len() > 24 {
            let drop_n = v.len() - 24;
            v.drain(..drop_n);
        }
    }
    (id, cancel)
}

fn task_progress(id: u64, done: u64, bytes: u64) {
    if let Ok(mut v) = TASKS.lock() {
        if let Some(t) = v.iter_mut().find(|t| t.id == id) {
            t.done = done;
            t.bytes = bytes;
        }
    }
}

/// 记一笔"这个文件是本次任务写出来的"，取消时要照着删回去。
fn task_created(id: u64, path: &str) {
    if let Ok(mut v) = TASKS.lock() {
        if let Some(t) = v.iter_mut().find(|t| t.id == id) {
            t.created.push(path.to_string());
        }
    }
}

/// 回滚：把本次任务新写的文件按倒序删掉，返回删掉的个数。
///
/// 只删清单里的路径 —— 用户原本就有的文件一个都不碰。
fn task_rollback(id: u64) -> usize {
    let list = {
        let Ok(mut v) = TASKS.lock() else { return 0 };
        let Some(t) = v.iter_mut().find(|t| t.id == id) else { return 0 };
        std::mem::take(&mut t.created)
    };
    let mut removed = 0usize;
    for p in list.iter().rev() {
        if std::fs::remove_file(p).is_ok() {
            removed += 1;
        }
        // 空出来的子目录顺手收掉（`libraries/com/foo/bar` 这种一层层建的）。
        if let Some(dir) = std::path::Path::new(p).parent() {
            let _ = std::fs::remove_dir(dir);
        }
    }
    removed
}

/// 把任务标成"已取消"，和"失败"区分开：界面上不该把用户自己的操作说成出错。
fn task_cancelled(id: u64, note: &str) {
    if let Ok(mut v) = TASKS.lock() {
        if let Some(t) = v.iter_mut().find(|t| t.id == id) {
            t.state = "cancelled";
            t.error = note.to_string();
        }
    }
}

fn task_finish(id: u64, ok: bool, err: &str) {
    if let Ok(mut v) = TASKS.lock() {
        if let Some(t) = v.iter_mut().find(|t| t.id == id) {
            t.state = if ok { "done" } else { "failed" };
            if ok {
                t.done = t.total.max(t.done);
                t.error.clear();
            } else {
                t.error = err.to_string();
            }
        }
    }
}

/// 请一个正在跑的任务收工。**只是置位**，不打断线程 —— 下载循环自己会在下一轮退出。
fn task_cancel(id: u64) -> bool {
    if let Ok(mut v) = TASKS.lock() {
        if let Some(t) = v.iter_mut().find(|t| t.id == id) {
            if t.state != "running" {
                // 已经在取消中：再点一次不算失败，别让界面弹"取消失败"。
                return t.state == "cancelling";
            }
            t.cancel.store(true, Ordering::SeqCst);
            t.state = "cancelling";
            return true;
        }
    }
    false
}


/// 给界面的任务快照。`running` 让外壳决定右下角的角标要不要显示。
fn tasks_json() -> String {
    let v = TASKS.lock().map(|g| {
        let mut out = String::from(r#"{"ok":true,"running":0,"tasks":["#);
        let mut running = 0u64;
        for (i, t) in g.iter().enumerate() {
            if t.state == "running" || t.state == "cancelling" {
                running += 1;
            }
            if i > 0 {
                out.push(',');
            }
            // 百分比在插件里算好：界面只管画，不重复推一遍算术。
            let percent = if t.total > 0 {
                ((t.done.min(t.total) * 1000) / t.total) as u32
            } else {
                0
            };
            out.push_str(
                &json!({
                    "id": t.id,
                    "kind": t.kind,
                    "title": t.title,
                    "state": t.state,
                    "done": t.done,
                    "total": t.total,
                    "bytes": t.bytes,
                    "percent": percent,
                    "cancellable": t.state == "running",
                    "started": t.started,
                    "error": t.error,
                })
                .to_string(),
            );
        }
        out.push_str(&format!("],\"runningCount\":{running}}}"));
        out
    });
    v.unwrap_or_else(|_| r#"{"ok":false,"error":"任务表读取失败"}"#.to_string())
}


struct State {
    host: MiyHostV1,
}

/// 允许把 host 搬到子线程：`MiyHostV1` 只是一组函数指针 + 一个 ctx 指针，
/// 宿主保证这些函数本身线程安全（内部全是锁）。
struct HostCell(MiyHostV1);
unsafe impl Send for HostCell {}
unsafe impl Sync for HostCell {}

impl HostCell {
    fn h(&self) -> &MiyHostV1 {
        &self.0
    }
}

// ---- 宿主服务的小封装 -----------------------------------------------------

pub(crate) fn call_json(h: &MiyHostV1, service: &str, req: &Value) -> Result<Value, String> {
    let body = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    let out = unsafe { h.call_json(service, &body) }.ok_or_else(|| format!("{service} 没有应答"))?;
    serde_json::from_slice(&out).map_err(|e| format!("{service} 返回的不是 JSON: {e}"))
}

/// 单次请求：错误里带一个「是不是中转站本身的问题」的标记。
fn fetch_json_once(h: &MiyHostV1, url: &str) -> Result<Value, (String, bool)> {
    let resp = call_json(
        h,
        "miy.net.get",
        &json!({
            "url": url,
            "timeout_ms": 30_000u64,
            "max_bytes": 8_388_608u64,
        }),
    )
    .map_err(|e| (e, true))?;
    if resp.get("ok").and_then(Value::as_bool) != Some(true) {
        let status = resp.get("status").and_then(Value::as_u64).unwrap_or(0);
        let err = resp
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("网络请求失败")
            .to_string();
        // 4xx（429 除外）是「这个请求本身不对」，不该连累中转站。
        let site_problem = status == 0 || status == 429 || status >= 500;
        return Err((err, site_problem));
    }
    let text = resp
        .get("text")
        .and_then(Value::as_str)
        .ok_or(("响应里没有 text".to_string(), true))?;
    serde_json::from_str(text).map_err(|e| (format!("JSON 解析失败: {e}"), false))
}

/// 按候选链依次试：主源 → 官方 → 自建中转（仅模组接口）。
/// 自建中转连续出问题会冷却一分钟。
fn fetch_json(h: &MiyHostV1, url: &str) -> Result<Value, String> {
    let mut last = String::from("没有可用的请求源");
    for c in candidates(url) {
        match fetch_json_once(h, &c) {
            Ok(v) => {
                if is_site(&c) {
                    site_recover();
                }
                return Ok(v);
            }
            Err((e, site_problem)) => {
                if is_site(&c) && site_problem {
                    site_fail();
                    site_dead_mark();
                }
                last = e;
            }
        }
    }
    Err(last)
}

/// 取纯文本（`maven-metadata.xml` 这类不是 JSON 的响应）。
fn fetch_text(h: &MiyHostV1, url: &str) -> Result<String, String> {
    let resp = call_json(
        h,
        "miy.net.get",
        &json!({
            "url": url,
            "timeout_ms": 30_000u64,
            "max_bytes": 8_388_608u64,
        }),
    )?;
    if resp.get("ok").and_then(Value::as_bool) != Some(true) {
        let status = resp.get("status").and_then(Value::as_u64).unwrap_or(0);
        let err = resp
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("网络请求失败");
        return Err(format!("{err}（HTTP {status}）"));
    }
    Ok(resp
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string())
}

/// 正在竞速的请求数：慢的那个线程还要跑好几秒，别无限堆。
static RACING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const RACE_MAX: usize = 6;

/// 多个候选源**并行**发，先到的赢。
///
/// 模组接口的两个镜像各有快慢（MCIM 的 Modrinth 快、CurseForge 搜索慢；自建中转
/// 反过来，热缓存 0.1 秒），串行试等于白等最慢的那个。都失败才回官方源。
fn fetch_json_race(h: &MiyHostV1, url: &str) -> Result<Value, String> {
    use std::sync::atomic::Ordering;
    let official = direct_of(url);
    let mut cands: Vec<String> = candidates(url)
        .into_iter()
        .filter(|c| c != &official)
        .collect();
    if cands.len() < 2 || RACING.load(Ordering::Relaxed) >= RACE_MAX {
        return fetch_json(h, url);
    }
    RACING.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = std::sync::mpsc::channel::<(String, Result<Value, (String, bool)>)>();
    for c in cands.drain(..) {
        let tx = tx.clone();
        let cell = HostCell(*h);
        std::thread::spawn(move || {
            let r = fetch_json_once(cell.h(), &c);
            let _ = tx.send((c, r));
        });
    }
    drop(tx);
    let mut last = String::from("没有可用的请求源");
    for (c, r) in rx {
        match r {
            Ok(v) => {
                RACING.fetch_sub(1, Ordering::Relaxed);
                if is_site(&c) {
                    site_recover();
                }
                return Ok(v);
            }
            Err((e, site_problem)) => {
                if is_site(&c) && site_problem {
                    site_fail();
                    site_dead_mark();
                }
                last = e;
            }
        }
    }
    RACING.fetch_sub(1, Ordering::Relaxed);
    if official != url {
        return fetch_json_once(h, &official)
            .map_err(|(e, _)| format!("{last}；官方源也失败：{e}"));
    }
    Err(last)
}

fn download_once(h: &MiyHostV1, url: &str, path: &str, size: u64) -> Result<u64, (String, bool)> {
    let mut req = json!({
        "url": url,
        "path": path,
        "timeout_ms": 90_000u64,
    });
    // 把期望大小一起给宿主：节点回 200 + 半截内容时当场判失败，别等 sha1 才发现。
    if size > 0 {
        req["size"] = json!(size);
    }
    let resp = call_json(h, "miy.net.download", &req).map_err(|e| (e, true))?;
    if resp.get("ok").and_then(Value::as_bool) != Some(true) {
        let status = resp.get("status").and_then(Value::as_u64).unwrap_or(0);
        let err = resp
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("下载失败")
            .to_string();
        let site_problem = status == 0 || status == 429 || status >= 500;
        return Err((format!("{url}: {err}"), site_problem));
    }
    Ok(resp.get("bytes").and_then(Value::as_u64).unwrap_or(0))
}

/// 整包下载：按候选链依次试（主源 → 官方 → 自建中转）。
///
/// 单个源返回 200 但内容不全（实测某个节点固定截在 8 MiB）时，宿主会带着期望大小
/// 判失败，这里就换下一个源 —— 而不是把半截文件当成成功。
fn download(h: &MiyHostV1, url: &str, path: &str, size: u64) -> Result<u64, String> {
    let mut last = String::from("没有可用的下载源");
    for c in candidates(url) {
        let mut tries = 0u32;
        loop {
            match download_once(h, &c, path, size) {
                Ok(n) => {
                    if is_site(&c) {
                        site_recover();
                    }
                    return Ok(n);
                }
                Err((e, site_problem)) => {
                    // 429 是「现在别来」，不是「这个源坏了」：等一会儿再来，
                    // 直接换源只会把限流带到下一个源上。
                    if e.contains("429") && tries < 2 {
                        tries += 1;
                        std::thread::sleep(std::time::Duration::from_secs(15 * tries as u64));
                        continue;
                    }
                    if is_site(&c) && site_problem {
                        site_fail();
                        site_dead_mark();
                    }
                    last = e;
                    break;
                }
            }
        }
    }
    Err(last)
}

/// 整包下载，但候选链由调用方给（`dl::Job.urls` 里已经排好镜像 → 官方）。
///
/// 分段下载走不通（服务器不认 Range）时用它：这时必须把整条候选链都试一遍，
/// 只试第一个源等于把"镜像抽风"变成"安装失败"。
pub(crate) fn download_any(
    h: &MiyHostV1,
    urls: &[String],
    path: &str,
    size: u64,
) -> Result<u64, String> {
    let mut last = String::from("没有可用的下载源");
    for c in urls {
        let mut tries = 0u32;
        loop {
            match download_once(h, c, path, size) {
                Ok(n) => {
                    if is_site(c) {
                        site_recover();
                    }
                    return Ok(n);
                }
                Err((e, site_problem)) => {
                    if e.contains("429") && tries < 2 {
                        tries += 1;
                        std::thread::sleep(std::time::Duration::from_secs(15 * tries as u64));
                        continue;
                    }
                    if is_site(c) && site_problem {
                        site_fail();
                        site_dead_mark();
                    }
                    last = e;
                    break;
                }
            }
        }
    }
    Err(last)
}

// ---- 版本清单 -------------------------------------------------------------

/// 模组版本清单缓存：详情页来回切模组时不用重复拉几百 KB。
/// 键是 `id|mc|loader`，最多留 24 条，5 分钟过期。
static MODVER_CACHE: Mutex<Vec<(String, std::time::Instant, Vec<u8>)>> = Mutex::new(Vec::new());

/// 版本清单缓存：拉一次能用两分钟。
///
/// 以前是十分钟，快照版一天能出好几个，用户经常遇到「官方已经 snapshot-8 了，
/// 启动器还停在 7」。清单只有 278 KB，两分钟一次不算负担。
static MAN_CACHE: Mutex<Option<(std::time::Instant, Value)>> = Mutex::new(None);
/// 正在拉清单的标记：别的线程别重复拉，等它拉完直接用。
static MAN_BUSY: Mutex<bool> = Mutex::new(false);
static MAN_DONE: Condvar = Condvar::new();
const MAN_TTL: std::time::Duration = std::time::Duration::from_secs(120);

fn man_cached() -> Option<Value> {
    let g = MAN_CACHE.lock().ok()?;
    let (at, v) = g.as_ref()?;
    if at.elapsed() < MAN_TTL {
        Some(v.clone())
    } else {
        None
    }
}

/// 缓存多久了（没有缓存就是 None）。
fn man_age() -> Option<std::time::Duration> {
    let g = MAN_CACHE.lock().ok()?;
    g.as_ref().map(|(at, _)| at.elapsed())
}

/// 后台把清单换成最新的，换完发事件让界面自己刷新。
///
/// 版本页打开时先用缓存（哪怕是旧的）立刻画出来，再在后台悄悄换新 ——
/// 用户不用盯着「正在加载」，新版本也不会因为缓存没过期就一直不出现。
fn refresh_manifest_bg(h: &MiyHostV1) {
    let raw = Box::into_raw(Box::new(*h)) as usize;
    std::thread::spawn(move || {
        let host = unsafe { &*(raw as *const MiyHostV1) };
        if let Ok(mut g) = MAN_CACHE.lock() {
            *g = None;
        }
        match manifest(host) {
            Ok(v) => {
                save_manifest_disk(host, &v);
                unsafe { host.emit_str("mc.versions.updated", b"{}") };
            }
            Err(e) => unsafe {
                host.log_str(MIY_LOG_WARN, &format!("miy.mc: 刷新版本清单失败：{e}"))
            },
        }
    });
}

fn manifest(h: &MiyHostV1) -> Result<Value, String> {
    if let Some(v) = man_cached() {
        return Ok(v);
    }
    // 已经有人在拉了：等它，但别无限等 —— 镜像挂了的时候要能退回本地清单。
    {
        let mut busy = MAN_BUSY.lock().map_err(|_| "清单锁坏了")?;
        while *busy {
            let (guard, _) = MAN_DONE
                .wait_timeout(busy, std::time::Duration::from_secs(3))
                .map_err(|_| "清单锁坏了")?;
            busy = guard;
            if *busy {
                // 等了 3 秒还没好：先用落盘的旧清单顶上，别让界面卡死。
                drop(busy);
                if let Some(v) = load_manifest_disk(h) {
                    return Ok(v);
                }
                return Err("版本清单暂时取不到，请检查网络后重试。".into());
            }
        }
        if let Some(v) = man_cached() {
            return Ok(v);
        }
        *busy = true;
    }
    let mut last = String::from("没有可用的清单地址");
    let mut got: Option<Value> = None;
    for url in manifest_urls(h) {
        match fetch_json(h, url) {
            Ok(v) => {
                got = Some(v);
                break;
            }
            Err(e) => last = format!("{url}: {e}"),
        }
    }
    if let Some(v) = &got {
        if let Ok(mut g) = MAN_CACHE.lock() {
            *g = Some((std::time::Instant::now(), v.clone()));
        }
    }
    if let Ok(mut busy) = MAN_BUSY.lock() {
        *busy = false;
    }
    MAN_DONE.notify_all();
    match got {
        Some(v) => Ok(v),
        None => {
            // 网络不通时退回上次落盘的清单，至少版本页还能用。
            if let Some(v) = load_manifest_disk(h) {
                if let Ok(mut g) = MAN_CACHE.lock() {
                    *g = Some((std::time::Instant::now(), v.clone()));
                }
                return Ok(v);
            }
            Err(last)
        }
    }
}

/// 清单落盘的位置：`{data}\cache\version_manifest.json`。
fn manifest_cache_file(h: &MiyHostV1) -> Option<std::path::PathBuf> {
    let info = unsafe { h.call_json("miy.app.info", b"{}") }?;
    let v: Value = serde_json::from_slice(&info).ok()?;
    let root = v.get("dataRoot").and_then(Value::as_str)?;
    Some(std::path::Path::new(root).join("cache").join("version_manifest.json"))
}

fn load_manifest_disk(h: &MiyHostV1) -> Option<Value> {
    let file = manifest_cache_file(h)?;
    let meta = std::fs::metadata(&file).ok()?;
    // 超过一天就当过期，别拿旧清单当真理。
    let age = meta.modified().ok()?.elapsed().unwrap_or_default();
    if age > std::time::Duration::from_secs(7 * 86400) {
        return None;
    }
    let bytes = std::fs::read(&file).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_manifest_disk(h: &MiyHostV1, v: &Value) {
    let Some(file) = manifest_cache_file(h) else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec(v) {
        let tmp = file.with_extension("json.tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &file);
        }
    }
}

/// 启动时在后台把清单拉好，用户点开版本页时就是现成的。
///
/// 宿主 vtable 里的函数指针本身是线程安全的（总线内部有锁），
/// 只是 `ctx: *mut c_void` 让编译器不敢放行，所以按地址传。
fn warm_manifest(h: &MiyHostV1) {
    // 宿主传进来的指针只在 start 调用期间有效，必须自己拷一份。
    let raw = Box::into_raw(Box::new(*h)) as usize;
    std::thread::spawn(move || {
        let host = unsafe { &*(raw as *const MiyHostV1) };
        // 先用上次落盘的清单把界面点亮。
        if let Some(v) = load_manifest_disk(host) {
            if let Ok(mut g) = MAN_CACHE.lock() {
                *g = Some((std::time::Instant::now(), v));
            }
        }
        // 然后**必须**真的去网上换一份：以前这里直接调 `manifest()`，
        // 而它看到内存里刚塞进去的落盘清单就原样返回，等于每次启动都在
        // 用旧清单 —— 快照版发了好几个，列表还停在老的。
        if let Ok(mut g) = MAN_CACHE.lock() {
            *g = None;
        }
        let t = std::time::Instant::now();
        match manifest(&host) {
            Ok(v) => {
                save_manifest_disk(host, &v);
                let n = v
                    .get("versions")
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0);
                let latest = v
                    .get("latest")
                    .and_then(|l| l.get("snapshot"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                unsafe {
                    host.log_str(
                        MIY_LOG_INFO,
                        &format!(
                            "miy.mc: 版本清单已预缓存（{n} 个版本，最新快照 {latest}，{} ms）",
                            t.elapsed().as_millis()
                        ),
                    );
                    host.emit_str("mc.versions.updated", b"{}");
                }
            }
            Err(e) => unsafe {
                host.log_str(MIY_LOG_WARN, &format!("miy.mc: 预缓存版本清单失败：{e}"))
            },
        }
    });
}

/// 版本 JSON 的地址：镜像优先，清单里那条官方地址兜底。
///
/// 清单里每个版本都自带 `url`（指向 piston-meta），镜像挂了就靠它 ——
/// 45 KB 的小文件，多试一次的成本可以忽略，但少了它镜像一抽风就装不了。
fn version_urls(man: &Value, id: &str) -> Vec<String> {
    let mut out = vec![format!("{}/version/{id}/json", mirror_base())];
    if let Some(u) = man
        .get("versions")
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter()
                .find(|v| v.get("id").and_then(Value::as_str) == Some(id))
        })
        .and_then(|v| v.get("url"))
        .and_then(Value::as_str)
    {
        if !out.iter().any(|x| x == u) {
            out.push(u.to_string());
        }
    }
    out
}

/// 解析一个版本，沿 `inheritsFrom` 向上合并（Fabric/Forge 就靠这条链）。
/// 在所有已登记的目录里找 `versions/{id}/{id}.json`。
fn local_version_any(h: &MiyHostV1, id: &str) -> Option<Value> {
    for root in all_roots(h) {
        let file = format!("{root}/versions/{id}/{id}.json");
        if let Ok(bytes) = std::fs::read(&file) {
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                return Some(v);
            }
        }
    }
    None
}

/// 装好的版本不需要清单：本地 json 在就直接用，省掉一次网络等待。
fn manifest_for(h: &MiyHostV1, id: &str) -> Value {
    if local_version_any(h, id).is_some() {
        return Value::Null;
    }
    manifest(h).unwrap_or(Value::Null)
}

fn resolve(h: &MiyHostV1, man: &Value, id: &str, depth: u32) -> Result<Value, String> {
    if depth > 5 {
        return Err(format!("{id}: inheritsFrom 层数过多"));
    }
    // 装好的版本先读本地 json：镜像挂了也能启动、也能看详情。
    let raw = match local_version_any(h, id) {
        Some(v) => v,
        None => {
            let urls = version_urls(man, id);
            if urls.is_empty() {
                return Err(format!("清单里没有版本 {id}"));
            }
            let mut last = String::new();
            let mut got = None;
            for u in &urls {
                match fetch_json(h, u) {
                    Ok(v) => {
                        got = Some(v);
                        break;
                    }
                    Err(e) => last = e,
                }
            }
            got.ok_or(last)?
        }
    };
    match raw.get("inheritsFrom").and_then(Value::as_str) {
        Some(parent_id) => {
            let parent = resolve(h, man, parent_id, depth + 1)?;
            Ok(merge(parent, raw))
        }
        None => Ok(raw),
    }
}

/// 这份 json 是不是「已经合并过加载器」的产物。
///
/// 合并后的 json 没有 `inheritsFrom`，主类是加载器的（Fabric 是 KnotClient）。
/// 拿它当底再合并一次，加载器的库会叠两遍，原版的 natives 也早就被去重掉了。
fn is_merged_loader(v: &Value) -> bool {
    if v.get("inheritsFrom").is_some() {
        return false;
    }
    let main = v.get("mainClass").and_then(Value::as_str).unwrap_or("");
    main.contains("fabric") || main.contains("quilt") || main.contains("knot")
}

/// 从镜像/官方取原版 json（不读本地），合并加载器时当底用。
fn fetch_vanilla(h: &MiyHostV1, man: &Value, id: &str) -> Result<Value, String> {
    let urls = version_urls(man, id);
    if urls.is_empty() {
        return Err(format!("清单里没有版本 {id}"));
    }
    let mut last = String::new();
    for u in &urls {
        match fetch_json(h, u) {
            Ok(v) => return Ok(v),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// 子版本覆盖父版本，但 `libraries` / `arguments` 是**追加**。
fn merge(parent: Value, child: Value) -> Value {    let mut out = match parent {
        Value::Object(m) => m,
        other => return other,
    };
    let Some(child_map) = child.as_object() else {
        return Value::Object(out);
    };
    for (k, v) in child_map {
        match k.as_str() {
            "inheritsFrom" => {}
            "libraries" => {
                let mut libs = out
                    .remove("libraries")
                    .and_then(|x| match x {
                        Value::Array(a) => Some(a),
                        _ => None,
                    })
                    .unwrap_or_default();
                if let Value::Array(more) = v {
                    libs.extend(more.iter().cloned());
                }
                out.insert("libraries".into(), Value::Array(libs));
            }
            "arguments" => {
                let mut args = out
                    .remove("arguments")
                    .and_then(|x| match x {
                        Value::Object(o) => Some(o),
                        _ => None,
                    })
                    .unwrap_or_default();
                if let Value::Object(more) = v {
                    for (kind, list) in more {
                        let entry = args
                            .entry(kind.clone())
                            .or_insert_with(|| Value::Array(Vec::new()));
                        if let (Value::Array(dst), Value::Array(src)) = (entry, list) {
                            dst.extend(src.iter().cloned());
                        }
                    }
                }
                out.insert("arguments".into(), Value::Object(args));
            }
            _ => {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// 同一个库的判定键：`group:artifact:classifier`，忽略版本号。
///
/// classifier 必须算进去：`org.lwjgl:lwjgl:3.4.3` 和
/// `org.lwjgl:lwjgl:3.4.3:natives-windows` 是**两个不同的文件**。
/// 只按 `group:artifact` 去重会把 26.3 的 56 个 natives 包全丢掉，
/// 表现就是 LWJGL 找不到 `lwjgl.dll`，Sodium 在 preLaunch 直接崩。
fn lib_key(name: &str) -> String {
    let mut it = name.split(':');
    let g = it.next().unwrap_or("");
    let a = it.next().unwrap_or("");
    let _version = it.next();
    let classifier = it.next().unwrap_or("");
    format!("{g}:{a}:{classifier}")
}

/// 把加载器的 profile 合进原版 json，得到一份**自包含**的版本配置。
///
/// 和 `merge()` 的区别：`merge()` 是"子版本叠在父版本上"（沿 `inheritsFrom` 解析时用，
/// 加载器的库追加在原版后面）；这里是**落盘**用的一次性合并 ——
/// 加载器的库必须排在原版前面：Fabric 自带 asm / mixin 等与原版同名不同版本的库，
/// 原版那份先上类路径会让加载器拿到错的字节码。
///
/// `mc` 是原版版本号，写进 `clientVersion`：合并后的 json 没有 `inheritsFrom` 了，
/// 版本页的 `describe()` 靠这个字段认出"这到底是哪个 MC 版本"。
fn merge_loader(base: &Value, prof: &Value, mc: &str) -> Value {
    let mut out = match base {
        Value::Object(m) => m.clone(),
        other => return other.clone(),
    };
    let Some(p) = prof.as_object() else {
        return Value::Object(out);
    };

    // 主类换成加载器的（Fabric 是 KnotClient）。
    if let Some(main) = p.get("mainClass") {
        out.insert("mainClass".into(), main.clone());
    }

    // 库：加载器在前，同 group:artifact 的原版库丢掉。
    let mut libs: Vec<Value> = p
        .get("libraries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut seen: std::collections::HashSet<String> = libs
        .iter()
        .filter_map(|l| l.get("name").and_then(Value::as_str).map(lib_key))
        .collect();
    for l in out
        .get("libraries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let keep = match l.get("name").and_then(Value::as_str) {
            Some(n) => seen.insert(lib_key(n)),
            None => true,
        };
        if keep {
            libs.push(l);
        }
    }
    out.insert("libraries".into(), Value::Array(libs));

    // 参数：加载器的在前、原版的后；一样的字符串只留一份（对象带 rules，全部保留）。
    let prof_args = p.get("arguments").cloned();
    let base_args = out.get("arguments").cloned();
    let mut args = serde_json::Map::new();
    for kind in ["jvm", "game"] {
        let mut list: Vec<Value> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for src in [prof_args.as_ref(), base_args.as_ref()] {
            let Some(Value::Object(o)) = src else { continue };
            let Some(Value::Array(a)) = o.get(kind) else {
                continue;
            };
            for v in a {
                if let Some(s) = v.as_str() {
                    if !seen.insert(s.to_string()) {
                        continue;
                    }
                }
                list.push(v.clone());
            }
        }
        if !list.is_empty() {
            args.insert(kind.to_string(), Value::Array(list));
        }
    }
    if !args.is_empty() {
        out.insert("arguments".into(), Value::Object(args));
    }

    out.insert("clientVersion".into(), Value::String(mc.to_string()));
    Value::Object(out)
}

// ---- 运行库与规则 ---------------------------------------------------------

fn rule_matches(rule: &Value) -> bool {
    // 我们没有任何可选特性（quickPlay、自定义分辨率、demo 模式都不支持），
    // 所以"要特性"的规则一律不命中。忽略这一条会让 `--quickPlayXxx` 同时出现
    // 好几个，游戏直接崩在 "Only one quick play option can be specified"。
    if rule.get("features").is_some() {
        return false;
    }
    let Some(os) = rule.get("os") else {
        return true;
    };
    if let Some(name) = os.get("name").and_then(Value::as_str) {
        if name != "windows" {
            return false;
        }
    }
    if let Some(arch) = os.get("arch").and_then(Value::as_str) {
        let x64 = cfg!(target_pointer_width = "64");
        if arch == "x86" && x64 {
            return false;
        }
        if arch == "x86_64" && !x64 {
            return false;
        }
    }
    true
}

/// Mojang 的规则语义：无规则 = 允许；否则按顺序匹配，最后一条命中的说了算。
fn allows(rules: Option<&Value>) -> bool {
    let Some(arr) = rules.and_then(Value::as_array) else {
        return true;
    };
    if arr.is_empty() {
        return true;
    }
    let mut allow = false;
    for r in arr {
        if !rule_matches(r) {
            continue;
        }
        allow = r.get("action").and_then(Value::as_str) == Some("allow");
    }
    allow
}

#[derive(Clone)]
pub(crate) struct Item {
    pub(crate) path: String,
    pub(crate) url: String,
    pub(crate) size: u64,
    /// 版本 JSON / 资源索引里给的 sha1；有就校验，没有就只校验大小。
    pub(crate) sha1: Option<String>,
}

/// 一个版本要下载的全部东西：运行库 + 客户端 jar + 资源索引。
/// 版本目录名 → 版本 json 的本地解析。装过的版本必须能离线启动，
/// 不能每次都去问远端清单（断网、清单改版都会把它卡死）。
/// 跨目录解析版本：加载器版本可能装在 `.minecraft`，而它继承的原版在另一个目录里。
///
/// 每一层都按 `roots` 顺序找同名 json，找到就用；找不到就停。
fn resolve_local_multi(roots: &[String], name: &str) -> Option<Value> {
    let mut child: Option<Value> = None;
    let mut cur = name.to_string();
    for _ in 0..6 {
        let mut found: Option<Value> = None;
        for r in roots {
            let p = format!("{r}\\versions\\{cur}\\{cur}.json");
            if let Ok(text) = std::fs::read_to_string(&p) {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    found = Some(v);
                    break;
                }
            }
        }
        let v = found?;
        let parent = v
            .get("inheritsFrom")
            .and_then(Value::as_str)
            .map(str::to_string);
        child = Some(match child {
            None => v,
            Some(c) => merge(v, c),
        });
        match parent {
            Some(pid) => cur = pid,
            None => return child,
        }
    }
    child
}

fn resolve_local(root: &str, name: &str) -> Option<Value> {
    let mut child: Option<Value> = None;
    let mut cur = name.to_string();
    for _ in 0..6 {
        let p = format!("{root}\\versions\\{cur}\\{cur}.json");
        let text = std::fs::read_to_string(&p).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        let parent = v
            .get("inheritsFrom")
            .and_then(Value::as_str)
            .map(str::to_string);
        child = Some(match child {
            None => v,
            Some(c) => merge(v, c),
        });
        match parent {
            Some(pid) => cur = pid,
            None => return child,
        }
    }
    child
}

/// 从 maven 坐标 `group:artifact:version[:classifier]` 推出仓库里的相对路径。
///
/// Fabric / Quilt 的 profile 只给 `name` + `url`，没有 `downloads`，
/// 得自己按 maven 规则拼路径，否则这些库既下不下来也进不了类路径。
fn maven_rel(name: &str) -> Option<String> {
    let parts: Vec<&str> = name.split(':').collect();
    if parts.len() < 3 {
        return None;
    }
    let (g, a, v) = (parts[0], parts[1], parts[2]);
    let cls = parts.get(3).copied().filter(|s| !s.is_empty());
    let file = match cls {
        Some(c) => format!("{a}-{v}-{c}.jar"),
        None => format!("{a}-{v}.jar"),
    };
    Some(format!("{}/{}/{}/{}", g.replace('.', "/"), a, v, file))
}

/// 一个库在 `libraries/` 下的相对路径、下载地址、大小和 sha1。
///
/// 优先用 JSON 里的 `downloads.artifact`；没有就按 maven 坐标 + `url` 推。
fn lib_entry(lib: &Value) -> Option<(String, String, u64, Option<String>)> {
    if let Some(art) = lib.get("downloads").and_then(|d| d.get("artifact")) {
        let path = art.get("path")?.as_str()?.to_string();
        let url = art.get("url")?.as_str()?.to_string();
        let size = art.get("size").and_then(Value::as_u64).unwrap_or(0);
        let sha1 = art.get("sha1").and_then(Value::as_str).map(str::to_string);
        return Some((path, url, size, sha1));
    }
    let name = lib.get("name").and_then(Value::as_str)?;
    let rel = maven_rel(name)?;
    let base = lib.get("url").and_then(Value::as_str).unwrap_or("https://libraries.minecraft.net/");
    let base = if base.ends_with('/') { base.to_string() } else { format!("{base}/") };
    Some((rel.clone(), format!("{base}{rel}"), 0, None))
}

fn plan(version: &Value, folder: &str) -> (Vec<Item>, Vec<Item>) {
    let mirror = mirror_base();
    let mut libs = Vec::new();
    if let Some(arr) = version.get("libraries").and_then(Value::as_array) {
        for lib in arr {
            if !allows(lib.get("rules")) {
                continue;
            }
            if let Some((rel, url, size, sha1)) = lib_entry(lib) {
                libs.push(Item {
                    path: format!("libraries/{rel}"),
                    url,
                    size,
                    sha1,
                });
            }
            // 老版本的原生库放在 classifiers 里，用 natives.windows 指名
            let native_key = lib
                .get("natives")
                .and_then(|n| n.get("windows"))
                .and_then(Value::as_str);
            if let Some(key) = native_key {
                if let Some(mut item) = lib
                    .get("downloads")
                    .and_then(|d| d.get("classifiers"))
                    .and_then(|c| c.get(key))
                    .and_then(item_of)
                {
                    item.path = format!("libraries/{}", item.path);
                    item.url = mirror_maven(&item.path);
                    libs.push(item);
                }
            }
        }
    }

    let mut extra = Vec::new();
    if let Some(client) = version.get("downloads").and_then(|d| d.get("client")) {
        let id = version.get("id").and_then(Value::as_str).unwrap_or("unknown");
        let size = client.get("size").and_then(Value::as_u64).unwrap_or(0);
        extra.push(Item {
            path: format!("versions/{folder}/{folder}.jar"),
            url: format!("{mirror}/version/{id}/client"),
            size,
            sha1: client.get("sha1").and_then(Value::as_str).map(str::to_string),
        });
    }
    if let Some(idx) = version.get("assetIndex") {
        let name = idx.get("id").and_then(Value::as_str).unwrap_or("legacy");
        let size = idx.get("size").and_then(Value::as_u64).unwrap_or(0);
        // 资源索引（几百 KB）是镜像没覆盖的唯一一个文件，只能走 JSON 里给的原地址。
        // 它只是一张 hash 清单，真正的 5000 多个资源对象都从镜像下。
        if let Some(url) = idx.get("url").and_then(Value::as_str) {
            extra.push(Item {
                path: format!("assets/indexes/{name}.json"),
                url: url.to_string(),
                size,
                sha1: idx.get("sha1").and_then(Value::as_str).map(str::to_string),
            });
        }
    }
    (libs, extra)
}

/// `libraries/<group>/<artifact>/<ver>/<file>.jar` → 镜像的 maven 路径。
fn mirror_maven(path: &str) -> String {
    let mirror = mirror_base();
    let rel = path.strip_prefix("libraries/").unwrap_or(path);
    format!("{mirror}/maven/{rel}")
}

fn item_of(v: &Value) -> Option<Item> {
    Some(Item {
        path: v.get("path")?.as_str()?.to_string(),
        url: v.get("url")?.as_str()?.to_string(),
        size: v.get("size").and_then(Value::as_u64).unwrap_or(0),
        sha1: v.get("sha1").and_then(Value::as_str).map(str::to_string),
    })
}

// ---- 下载 ----------------------------------------------------------------

/// 内置的 CDN 后端地址。换后端改这一行就行，不需要让用户填。
///
/// 桶是私有的，所以启动器不能直接拼 URL —— 得先向后端要一个**预签名地址**。
/// 后端拿 AK/SK 签名，启动器只拿到一个 15 分钟有效的 URL，密钥永远不出服务器。
const CDN_BACKEND: &str = "https://dash.miyui.vip:8443";

/// 后端连不上就整轮跳过 —— 一个挂掉的后端不该让每次安装都多等几秒。
static CDN_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn cdn_base(h: &MiyHostV1) -> String {
    // 环境变量（调试用）> settings.json 里的 miy.mc.cdn（界面上不显示，留给排障）> 内置地址
    if let Ok(v) = std::env::var("MIY_CDN") {
        return v.trim_end_matches('/').to_string();
    }
    let snap = settings_snapshot(h);
    let s = setting_str(&snap, "miy.mc.cdn");
    if !s.is_empty() {
        return s.trim_end_matches('/').to_string();
    }
    CDN_BACKEND.trim_end_matches('/').to_string()
}

/// 一次向后端要一批预签名地址：`{key: url}`。
///
/// 后端只返回它**确实有**的对象，所以"CDN 上没这个文件"是正常情况，
/// 调用方拿到空表就继续走镜像/官方，不需要额外判断。
fn cdn_presign(h: &MiyHostV1, base: &str, keys: &[String]) -> std::collections::HashMap<String, String> {
    use std::sync::atomic::Ordering;
    let mut out = std::collections::HashMap::new();
    if base.is_empty() || keys.is_empty() || CDN_DOWN.load(Ordering::Relaxed) {
        return out;
    }
    // 后端一次最多签 2000 个 key，按这个上限分批：5000 多个资源对象 3 次请求就够。
    // 分批太小会让"每次都要过一次隧道"的延迟被放大好几倍。
    for chunk in keys.chunks(2000) {
        let req = json!({ "keys": chunk });
        let r = call_json(h, "miy.net.post", &json!({
            "url": format!("{base}/api/presign"),
            "body": serde_json::to_string(&req).unwrap_or_default(),
            // 隧道到源站单程就要 1–4 秒（实测 /health 也一样），4 秒必然误判。
            "timeout_ms": 20_000u64,
        }));
        let Ok(v) = r else {
            // 后端没起来 / 域名解析不了：这一轮别再试了，直接走镜像。
            CDN_DOWN.store(true, Ordering::Relaxed);
            unsafe {
                h.log_str(MIY_LOG_DEBUG, "CDN 后端连不上，本轮跳过（不影响下载）");
            }
            return out;
        };
        // `miy.net.post` 返回的是 `{ok, status, text, bytes}` —— 后端的 JSON 在 text 里。
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            // 404/500 说明后端没部署好，别每批都去撞一次。
            CDN_DOWN.store(true, Ordering::Relaxed);
            unsafe {
                h.log_str(MIY_LOG_DEBUG, "CDN 后端返回异常，本轮跳过（不影响下载）");
            }
            return out;
        }
        let Some(text) = v.get("text").and_then(Value::as_str) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        let Some(urls) = parsed.get("urls").and_then(Value::as_object) else {
            continue;
        };
        for (k, u) in urls {
            if let Some(s) = u.as_str() {
                out.insert(k.clone(), s.to_string());
            }
        }
    }
    out
}

/// 并行下完所有条目。`concurrency` 来自设置，默认 4；每 `step` 个发一次进度。
/// 一批下载跑完之后的样子。
pub(crate) struct Fetched {
    pub(crate) errors: Vec<String>,
    #[allow(dead_code)]
    pub(crate) cancelled: bool,
    #[allow(dead_code)]
    pub(crate) done: usize,
}

/// 可取消的单文件下载：交给分块引擎，取消时引擎会删掉 `.part`。
///
/// `urls` 是候选源，引擎按块换源重试；只有一个源时就是普通下载。
fn download_cancellable(
    h: &MiyHostV1,
    urls: &[String],
    path: &str,
    size: u64,
    cancel: &AtomicBool,
) -> Result<u64, String> {
    if cancel.load(Ordering::SeqCst) {
        return Err("已取消".into());
    }
    let job = dl::Job::with_sources(urls.to_vec(), path, size, None);
    let rep = dl::run(h, vec![job], 8, Some(cancel), &|_, _| {}, &|_| {});
    if rep.cancelled || cancel.load(Ordering::SeqCst) {
        return Err("已取消".into());
    }
    if let Some(e) = rep.errors.into_iter().next() {
        return Err(e);
    }
    Ok(rep.bytes)
}

/// Modrinth 文件的候选源：官方 CDN + MCIM 镜像。
fn modrinth_urls(url: &str) -> Vec<String> {
    let mut v = vec![url.to_string()];
    if let Some(rest) = url.strip_prefix("https://cdn.modrinth.com") {
        v.push(format!("{MCIM}{rest}"));
    }
    // CurseForge 的文件 CDN 同理（`mediafilez.forgecdn.net` 不在镜像范围内，别一起换）。
    if let Some(rest) = url.strip_prefix("https://edge.forgecdn.net") {
        v.push(format!("{MCIM}{rest}"));
    }
    v
}

/// 一个条目的候选源，按优先级排列。
///
/// 顺序是「中转站 → 官方直连 → 额外镜像」：中转站最快但可能挂，官方直连最稳但慢，
/// 额外镜像（MCIM / 阿里云 maven）只在前面都失败时才试。引擎按块换源重试，
/// 所以这里给的是"整条链"，不是"选一个"。
fn candidates_for(it: &Item) -> Vec<String> {
    let direct = direct_of(&it.url);
    let mut out = candidates(&it.url);
    // MCIM：Modrinth CDN 的国内镜像，路径规则是「把 cdn.modrinth.com 换成 mod.mcimirror.top」。
    if let Some(rest) = direct.strip_prefix("https://cdn.modrinth.com") {
        let m = format!("{MCIM}{rest}");
        if !out.iter().any(|u| u == &m) {
            out.push(m);
        }
    }
    // CurseForge 的文件 CDN 同理（`edge.forgecdn.net` → `mod.mcimirror.top`）。
    // 注意 `mediafilez.forgecdn.net` 不在镜像范围内，别一起换。
    if let Some(rest) = direct.strip_prefix("https://edge.forgecdn.net") {
        let m = format!("{MCIM}{rest}");
        if !out.iter().any(|u| u == &m) {
            out.push(m);
        }
    }
    // 阿里云 maven：Maven Central 的国内镜像（libraries 里有一部分来自中央仓库）。
    if let Some(rest) = direct.strip_prefix("https://libraries.minecraft.net/") {
        let m = format!("https://maven.aliyun.com/repository/public/{rest}");
        if !out.iter().any(|u| u == &m) {
            out.push(m);
        }
    }
    // BMCLAPI 同时提供 `/maven` 与 `/libraries` 两条路径指向同一批文件。
    // 一条抽风时另一条通常还通，多给一条的成本几乎为零。
    if let Some(rest) = direct.strip_prefix(BMCLAPI) {
        let alt = if let Some(p) = rest.strip_prefix("/maven/") {
            Some(format!("{BMCLAPI}/libraries/{p}"))
        } else if let Some(p) = rest.strip_prefix("/libraries/") {
            Some(format!("{BMCLAPI}/maven/{p}"))
        } else {
            None
        };
        if let Some(a) = alt {
            if !out.iter().any(|u| u == &a) {
                out.push(a);
            }
        }
    }
    // 客户端 jar：官方地址是 piston-data 的 `objects/<sha1>/client.jar`。
    // **排在最前面**：41 MB 的文件要开 8 条连接分段，而镜像按节点限流
    // （实测 8 条并发打同一个 client 会连续 429，白等一分钟），官方 CDN
    // 没有这个限制（实测 8 条并发 3.2 MB/s）。镜像留作兜底。
    if it.path.starts_with("versions/") && it.path.ends_with(".jar") {
        if let Some(sha) = &it.sha1 {
            let o = format!("https://piston-data.mojang.com/v1/objects/{sha}/client.jar");
            if !out.iter().any(|u| u == &o) {
                out.insert(0, o);
            }
        }
    }
    out.retain(|u| !no_range(u));
    out
}

/// 并发下载一批文件。
///
/// 三条规矩是踩坑换来的：
/// 1. **单个文件失败不 abort，重试 3 次**（退避 400/800 ms）。镜像偶尔抽一下，
///    整批报废对用户来说就是"卡在第 64 个文件然后报错"。
/// 2. **取消是协作式的**：只看一个 `AtomicBool`，下一轮循环退出，不打断正在
///    读的 socket（打断只会留下半截文件）。
/// 3. **大小对得上就跳过**，所以重跑是安全的、断点续传也是免费的。
pub(crate) fn fetch_all(
    h: &MiyHostV1,
    root: &str,
    items: &[Item],
    concurrency: usize,
    _step: usize,
    task: Option<u64>,
    cancel: Option<&AtomicBool>,
) -> Fetched {
    let total = items.len();
    if total == 0 {
        return Fetched { errors: Vec::new(), cancelled: false, done: 0 };
    }
    let cell = HostCell(*h);
    let done = AtomicUsize::new(0);

    // 记下「这个文件本来不存在」：取消回滚时只删自己造的。
    // 自建 CDN：先要一批预签名地址。有的话排在最前面 —— 它最快，也不占镜像额度。
    let cdn = cdn_base(h);
    unsafe {
        h.log_str(MIY_LOG_DEBUG, &format!("CDN 后端：{cdn:?}"));
    }
    let presigned = if cdn.is_empty() {
        std::collections::HashMap::new()
    } else {
        let keys: Vec<String> = items.iter().map(|it| it.path.clone()).collect();
        let m = cdn_presign(h, &cdn, &keys);
        if !m.is_empty() {
            unsafe {
                h.log_str(
                    MIY_LOG_DEBUG,
                    &format!("CDN 预签名 {} / {} 个文件", m.len(), keys.len()),
                );
            }
        }
        m
    };

    let mut existed = Vec::with_capacity(total);
    let mut jobs = Vec::with_capacity(total);
    for it in items {
        let full = format!("{root}/{}", it.path);
        existed.push(std::path::Path::new(&full).is_file());
        let mut urls = candidates_for(it);
        if let Some(u) = presigned.get(&it.path) {
            urls.insert(0, u.clone());
        }
        jobs.push(dl::Job {
            urls,
            path: full,
            size: it.size,
            sha1: it.sha1.clone(),
        });
    }

    let mut rep = dl::run(
        h,
        jobs,
        concurrency,
        cancel,
        &|b, t| {
            let d = done.load(Ordering::Relaxed);
            if let Some(t) = task {
                task_progress(t, d as u64, b);
            }
            // 同时给「文件数」和「字节数」：最后一个大文件（客户端 jar）下载时，
            // 文件数早就停在 99%，只有字节数还在动 —— 界面按字节画才不会像卡死。
            let payload =
                format!(r#"{{"done":{d},"total":{total},"bytes":{b},"totalBytes":{t}}}"#);
            unsafe { cell.h().emit_str("mc.progress", payload.as_bytes()) };
        },
        &|i| {
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            if !existed.get(i).copied().unwrap_or(true) {
                if let Some(t) = task {
                    let full = format!("{root}/{}", items[i].path);
                    task_created(t, &full);
                }
            }
            if d == total {
                if let Some(t) = task {
                    task_progress(t, d as u64, 0);
                }
            }
        },
    );

    // 校验失败几乎总是"镜像给了错内容"（半截文件、错误页、旧版本），不是用户的问题。
    // 整批重来一次、只重试这些文件：用户该看到的是"重试后成功"，不是"校验失败"。
    if !rep.cancelled {
        let bad: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, it)| {
                let full = format!("{root}/{}", it.path);
                rep.errors.iter().any(|e| e.contains(&full) && e.contains("校验失败"))
            })
            .map(|(i, _)| i)
            .collect();
        if !bad.is_empty() {
            let jobs2: Vec<dl::Job> = bad
                .iter()
                .map(|&i| dl::Job {
                    urls: candidates_for(&items[i]),
                    path: format!("{root}/{}", items[i].path),
                    size: items[i].size,
                    sha1: items[i].sha1.clone(),
                })
                .collect();
            let rep2 = dl::run(h, jobs2, concurrency, cancel, &|_, _| {}, &|_| {});
            // 重试过的从旧错误里摘掉；重试仍失败的由 rep2 重新报。
            rep.errors.retain(|e| {
                !(e.contains("校验失败")
                    && bad
                        .iter()
                        .any(|&i| e.contains(&format!("{root}/{}", items[i].path))))
            });
            rep.errors.extend(rep2.errors);
            rep.done += rep2.done;
            rep.bytes += rep2.bytes;
        }
    }
    if let Some(t) = task {
        task_progress(t, done.load(Ordering::Relaxed) as u64, rep.bytes);
    }
    Fetched {
        errors: rep.errors,
        cancelled: rep.cancelled,
        done: rep.done,
    }
}

// ---- 请求参数 -------------------------------------------------------------

fn body_json(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

fn field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

// ---- 游戏目录（.minecraft）：可以登记多个，和 PCL2 一样 ---------------------

/// 界面状态（比如首页选中的版本）：存在数据目录的 config 里。
///
/// 为什么需要它：切到别的页面时外壳会把 iframe 从 DOM 上摘下来，再切回来
/// 浏览器会重新加载 `srcdoc`，页面里的变量全丢 —— 用户选好的版本就跳回第一个。
/// 这类"用户选了什么"必须落到磁盘上，回来时才接得上。
fn ui_state_file(h: &MiyHostV1) -> Option<std::path::PathBuf> {
    let info = call_json(h, "miy.app.info", &json!({})).ok()?;
    let data = info.get("dataRoot").and_then(Value::as_str)?;
    Some(std::path::Path::new(data).join("config").join("ui-state.json"))
}

fn service_state_get(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let key = field(req, "key").unwrap_or("");
    let v = ui_state_file(h)
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or(Value::Null);
    Ok(json!({"ok": true, "value": v.get(key).cloned().unwrap_or(Value::Null)})
        .to_string()
        .into_bytes())
}

fn service_state_set(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let key = field(req, "key").unwrap_or("");
    if key.is_empty() {
        return Ok(json!({"ok": false, "error": "缺少 key"}).to_string().into_bytes());
    }
    let Some(file) = ui_state_file(h) else {
        return Ok(json!({"ok": false, "error": "拿不到数据目录"}).to_string().into_bytes());
    };
    let mut v = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_else(|| json!({}));
    if !v.is_object() {
        v = json!({});
    }
    if let Some(o) = v.as_object_mut() {
        o.insert(
            key.to_string(),
            req.get("value").cloned().unwrap_or(Value::Null),
        );
    }
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::write(&file, serde_json::to_vec_pretty(&v).unwrap_or_default()) {
        Ok(()) => Ok(json!({"ok": true}).to_string().into_bytes()),
        Err(e) => Ok(json!({"ok": false, "error": format!("写入失败：{e}")})
            .to_string()
            .into_bytes()),
    }
}

/// 登记过的 `.minecraft` 目录清单，存在数据目录的 config 里。
fn roots_file(h: &MiyHostV1) -> Option<std::path::PathBuf> {
    let info = call_json(h, "miy.app.info", &json!({})).ok()?;
    let data = info.get("dataRoot").and_then(Value::as_str)?;
    Some(std::path::Path::new(data).join("config").join("roots.json"))
}

fn load_roots(h: &MiyHostV1) -> Vec<String> {
    let Some(file) = roots_file(h) else { return Vec::new() };
    let Ok(bytes) = std::fs::read(&file) else { return Vec::new() };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else { return Vec::new() };
    v.get("roots")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn save_roots(h: &MiyHostV1, roots: &[String]) {
    let Some(file) = roots_file(h) else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = json!({ "roots": roots });
    if let Ok(bytes) = serde_json::to_vec_pretty(&body) {
        let _ = std::fs::write(&file, bytes);
    }
}

/// 默认目录：启动器本体旁边的 `MC`（保持老用户的数据不动）。
fn default_root(h: &MiyHostV1) -> String {
    let info = call_json(h, "miy.app.info", &json!({})).ok();
    if let Some(dir) = info
        .as_ref()
        .and_then(|v| v.get("exeDir").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
    {
        // 和 PCL2 一样用 `.minecraft` 当默认目录，方便和官方启动器、其他启动器共用。
        return format!("{dir}\\.minecraft");
    }
    let data = info
        .and_then(|v| v.get("dataRoot").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| ".".to_string());
    format!("{data}\\minecraft")
}

/// 所有要扫的目录：默认目录 + 登记过的 + 老版本的 `MC`（升级后别让旧版本消失）。
fn all_roots(h: &MiyHostV1) -> Vec<String> {
    let mut out = vec![default_root(h)];
    // 老版本把游戏装在 exe 旁边的 `MC` 里：目录还在就继续认，用户不用手动搬。
    if let Some(dir) = call_json(h, "miy.app.info", &json!({}))
        .ok()
        .and_then(|v| v.get("exeDir").and_then(Value::as_str).map(str::to_string))
    {
        let legacy = format!("{dir}\\MC");
        if std::path::Path::new(&legacy).join("versions").is_dir() {
            out.push(legacy);
        }
    }
    for r in load_roots(h) {
        if !out.iter().any(|x| x.eq_ignore_ascii_case(&r)) {
            out.push(r);
        }
    }
    out
}

/// 这个版本在哪个 `.minecraft` 里（按目录名找，找不到返回 None）。
fn root_of_version(h: &MiyHostV1, id: &str) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    for root in all_roots(h) {
        if std::path::Path::new(&root).join("versions").join(id).is_dir() {
            return Some(root);
        }
    }
    None
}

/// 游戏目录：请求里的 `root` > 这个版本所在的目录 > 设置 `miy.mc.root` > 默认目录。
fn game_root(h: &MiyHostV1, req: &Value) -> String {
    if let Some(r) = field(req, "root") {
        return r.to_string();
    }
    for key in ["id", "name", "from"] {
        if let Some(id) = field(req, key) {
            if let Some(root) = root_of_version(h, id) {
                return root;
            }
        }
    }
    if let Ok(snap) = call_json(h, "miy.app.settings", &json!({})) {
        if let Some(r) = snap
            .get("values")
            .and_then(|v| v.get("miy.mc.root"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return r.to_string();
        }
    }
    default_root(h)
}

// ---- 目录管理：新建 / 添加 / 移除 .minecraft --------------------------------

/// 一个目录像不像 `.minecraft`：有 versions / assets / libraries 里任意一个就算。
fn looks_like_mc(dir: &std::path::Path) -> bool {
    ["versions", "assets", "libraries", "mods", "saves"]
        .iter()
        .any(|k| dir.join(k).is_dir())
}

/// 侧栏里显示的名字：`.minecraft` 这种通用名带上上级目录，免得几个目录同名。
fn root_label(p: &std::path::Path) -> String {
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned());
    if name.eq_ignore_ascii_case(".minecraft") {
        if let Some(parent) = p.parent().and_then(|q| q.file_name()) {
            return format!("{}\\.minecraft", parent.to_string_lossy());
        }
    }
    name
}

fn service_roots(h: &MiyHostV1, _req: &Value) -> Result<Vec<u8>, String> {
    let default = default_root(h);
    let mut out = Vec::new();
    for root in all_roots(h) {
        let p = std::path::Path::new(&root);
        let versions = std::fs::read_dir(p.join("versions"))
            .map(|it| it.flatten().filter(|e| e.path().is_dir()).count())
            .unwrap_or(0);
        out.push(json!({
            "path": root,
            "name": p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| root.clone()),
            // 好几个目录都叫 .minecraft 时，光看名字分不清谁是谁，带上上级目录名。
            "label": root_label(p),
            "versions": versions,
            "exists": p.is_dir(),
            "isDefault": root.eq_ignore_ascii_case(&default),
        }));
    }
    Ok(serde_json::to_vec(&json!({ "ok": true, "default": default, "roots": out })).unwrap_or_default())
}

/// 新建或添加一个 `.minecraft` 目录：`create` 为真时把标准子目录建出来。
fn service_root_add(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let path = field(req, "path").unwrap_or("").trim().to_string();
    if path.is_empty() {
        return Err("缺少目录".into());
    }
    let dir = std::path::Path::new(&path);
    let create = req.get("create").and_then(Value::as_bool).unwrap_or(false);
    if !dir.is_dir() {
        if !create {
            return Err(format!("目录不存在：{path}"));
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("无法创建目录：{e}"))?;
    }
    if create {
        // 和 PCL2 一样：根目录只放公共资源，mods / saves 这些都在各自的版本目录里（版本隔离）。
        for sub in ["versions", "assets", "libraries"] {
            let _ = std::fs::create_dir_all(dir.join(sub));
        }
    } else if !looks_like_mc(dir) {
        return Err("这个目录看起来不是 .minecraft（里面没有 versions / assets / libraries）。".into());
    }
    let mut roots = load_roots(h);
    if !roots.iter().any(|r| r.eq_ignore_ascii_case(&path)) {
        roots.push(path.clone());
        save_roots(h, &roots);
    }
    Ok(serde_json::to_vec(&json!({ "ok": true, "path": path })).unwrap_or_default())
}

/// 从列表里移除；`delete` 为真时连磁盘上的文件夹一起删掉。
fn service_root_remove(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let path = field(req, "path").unwrap_or("").trim().to_string();
    if path.is_empty() {
        return Err("缺少目录".into());
    }
    let delete = req.get("delete").and_then(Value::as_bool).unwrap_or(false);
    if delete {
        let dir = std::path::Path::new(&path);
        if !dir.is_dir() {
            return Err(format!("目录不存在：{path}"));
        }
        // 两道安全阀：不删盘符根，也不删启动器自己的默认目录。
        if dir.parent().is_none() {
            return Err("不能删除磁盘根目录。".into());
        }
        if path.eq_ignore_ascii_case(&default_root(h)) {
            return Err("这是启动器默认的游戏目录，不能删除。".into());
        }
        std::fs::remove_dir_all(dir).map_err(|e| format!("删除失败：{e}"))?;
    }
    let roots: Vec<String> = load_roots(h)
        .into_iter()
        .filter(|r| !r.eq_ignore_ascii_case(&path))
        .collect();
    save_roots(h, &roots);
    Ok(serde_json::to_vec(&json!({ "ok": true, "path": path, "deleted": delete })).unwrap_or_default())
}

/// 重命名游戏目录：磁盘上的文件夹一起改名，登记表同步更新。
fn service_root_rename(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let path = field(req, "path").unwrap_or("").trim().to_string();
    let name = field(req, "name").unwrap_or("").trim().to_string();
    if path.is_empty() {
        return Err("缺少目录".into());
    }
    if name.is_empty() {
        return Err("名字不能为空。".into());
    }
    if name == "." || name == ".." || name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
        return Err("名字里不能有 \\ / : * ? \" < > | 这些字符。".into());
    }
    let old = std::path::Path::new(&path);
    if !old.is_dir() {
        return Err(format!("目录不存在：{path}"));
    }
    let parent = old.parent().ok_or("这个目录没有上级目录，不能改名。")?;
    let new = parent.join(&name);
    if new == old {
        return Ok(serde_json::to_vec(&json!({ "ok": true, "path": path })).unwrap_or_default());
    }
    if new.exists() {
        return Err(format!("{} 已经存在了。", new.display()));
    }
    std::fs::rename(old, &new).map_err(|e| format!("改名失败：{e}"))?;
    let new_path = new.to_string_lossy().into_owned();
    let roots: Vec<String> = load_roots(h)
        .into_iter()
        .map(|r| if r.eq_ignore_ascii_case(&path) { new_path.clone() } else { r })
        .collect();
    save_roots(h, &roots);
    Ok(serde_json::to_vec(&json!({ "ok": true, "path": new_path })).unwrap_or_default())
}

/// 导入整合包：认我们自己导出的 `modpack.json`，也认只有 mods/config 的普通压缩包。
fn service_import(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let path = field(req, "path").unwrap_or("").trim().to_string();
    if path.is_empty() {
        return Err("缺少整合包路径".into());
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("读不了这个文件：{e}"))?;
    // `.mrpack` 走 Modrinth 那条路：它自带清单，要连原版和加载器一起装，
    // 跟"把 mods 铺进一个已经装好的版本"是两件事。
    if pack::is_mrpack(&bytes) {
        let root = game_root(h, req);
        return modpack_install_bytes(h, req, &root, &bytes);
    }
    let entries = zip_list(&bytes).map_err(|e| format!("这不是有效的整合包：{e}"))?;
    let find = |name: &str| entries.iter().find(|e| e.name.eq_ignore_ascii_case(name));

    let mut mc = String::new();
    let mut pack_name = std::path::Path::new(&path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "整合包".to_string());
    if let Some(e) = find("modpack.json") {
        if let Ok(raw) = zip_read(&bytes, e) {
            if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
                mc = v.get("mc").and_then(Value::as_str).unwrap_or("").to_string();
                if let Some(n) = v.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    pack_name = n.to_string();
                }
            }
        }
    }
    if mc.is_empty() {
        return Err("这个压缩包里没有 modpack.json，认不出对应的 Minecraft 版本。".into());
    }
    let Some(root) = root_of_version(h, &mc) else {
        return Err(format!("请先安装 Minecraft {mc}，再导入这个整合包。"));
    };
    let clean: String = pack_name
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let versions = std::path::Path::new(&root).join("versions");
    let mut name = clean.clone();
    let mut n = 2;
    while versions.join(&name).exists() {
        name = format!("{clean}-{n}");
        n += 1;
    }
    let dir = versions.join(&name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建版本目录：{e}"))?;
    // 版本隔离：导入的整合包也按 PCL2 的布局建好每个版本自己的目录。
    for sub in ["mods", "saves", "config", "logs", "resourcepacks", "shaderpacks", "crash-reports"] {
        let _ = std::fs::create_dir_all(dir.join(sub));
    }

    let mut files = 0u32;
    for e in &entries {
        let rel = e.name.replace('\\', "/");
        let keep = rel.starts_with("mods/")
            || rel.starts_with("config/")
            || rel.starts_with("saves/")
            || rel.starts_with("resourcepacks/")
            || rel.starts_with("shaderpacks/")
            || rel == "options.txt";
        if !keep || rel.ends_with('/') {
            continue;
        }
        let Ok(data) = zip_read(&bytes, e) else { continue };
        let target = dir.join(rel.replace('/', "\\"));
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&target, data).is_ok() {
            files += 1;
        }
    }
    let meta = json!({
        "id": name,
        "inheritsFrom": mc,
        "type": "release",
        "releaseTime": "1970-01-01T00:00:00+00:00",
    });
    let json_path = dir.join(format!("{name}.json"));
    std::fs::write(&json_path, serde_json::to_vec_pretty(&meta).unwrap_or_default())
        .map_err(|e| format!("无法写入版本信息：{e}"))?;
    unsafe {
        h.emit_str("mc.imported", format!(r#"{{"name":"{name}"}}"#).as_bytes());
    }
    Ok(serde_json::to_vec(&json!({
        "ok": true,
        "name": name,
        "root": root,
        "mc": mc,
        "files": files,
    }))
    .unwrap_or_default())
}

/// 整合包本体：本地路径直接读，URL 先下到 `cache/packs` 再读。
fn pack_bytes(h: &MiyHostV1, req: &Value, root: &str) -> Result<Vec<u8>, String> {
    if let Some(p) = field(req, "path").map(str::trim).filter(|s| !s.is_empty()) {
        return std::fs::read(p).map_err(|e| format!("读不了这个整合包：{e}"));
    }
    let url = field(req, "url")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("缺少整合包路径或下载地址")?;
    let dir = format!("{root}\\cache\\packs");
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建缓存目录：{e}"))?;
    let raw = url.rsplit('/').next().unwrap_or("pack.mrpack");
    let name: String = raw
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let file = format!("{dir}\\{name}");
    download(h, url, &file, 0)?;
    std::fs::read(&file).map_err(|e| format!("读不了下载好的整合包：{e}"))
}

/// 导入 Modrinth 整合包（`.mrpack`）：一次装成一个能直接启动的版本。
///
/// 顺序是「先装原版 + 加载器，再往实例目录里铺文件」。装原版复用 `service_install`
/// （它已经会合并加载器、复用本地 jar、发进度），所以整合包实例和手动装的版本
/// 在磁盘上是同一种东西，启动路径不用分叉。
fn service_modpack_install(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let root = game_root(h, req);
    let bytes = pack_bytes(h, req, &root)?;
    modpack_install_bytes(h, req, &root, &bytes)
}

/// 已经拿到包体之后的安装流程（`service_import` 认出 `.mrpack` 时也走这里，
/// 免得同一个文件读两遍）。
fn modpack_install_bytes(
    h: &MiyHostV1,
    req: &Value,
    root: &str,
    bytes: &[u8],
) -> Result<Vec<u8>, String> {
    let pack = pack::read_index(bytes)?;
    if !pack.loader.is_empty() && !matches!(pack.loader.as_str(), "fabric" | "quilt") {
        return Err(format!(
            "暂不支持 {} 整合包（目前只支持 Fabric / Quilt）",
            pack.loader
        ));
    }

    // 版本目录名：用户指定 > 包名。重名就加序号，绝不覆盖已经装好的实例
    // （整合包里的存档、配置是用户的东西，覆盖等于删档）。
    let base: String = field(req, "name")
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| pack.name.clone())
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let base = base.trim().to_string();
    let base = if base.is_empty() { format!("{} 整合包", pack.mc) } else { base };
    let versions = std::path::Path::new(root).join("versions");
    let mut name = base.clone();
    let mut n = 2;
    while versions.join(&name).exists() {
        name = format!("{base}-{n}");
        n += 1;
    }

    // 原版是整合包实例的"底座"：实例 json 用 `inheritsFrom` 借它的客户端 jar
    // 和运行库，所以原版必须真的在磁盘上。装过就直接用 —— 不重复校验，
    // 否则每装一个整合包都要把 41 MB 的 jar 和 60 个库再核一遍。
    let base_dir = versions.join(&pack.mc);
    let base_ok = base_dir.join(format!("{}.json", pack.mc)).is_file()
        && base_dir.join(format!("{}.jar", pack.mc)).is_file()
        && !base_dir.join(".incomplete").is_file();
    if !base_ok {
        let ireq = json!({ "id": pack.mc, "name": pack.mc, "root": root });
        let out = service_install(h, &ireq)?;
        let outv: Value = serde_json::from_slice(&out).unwrap_or(Value::Null);
        if !outv.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            // 原版都没装成，整合包的文件下了也没用：把安装的错误原样交给界面。
            return Ok(out);
        }
    }

    // 实例目录：只放这个包自己的东西（mods/config/存档），jar 与运行库都借原版的。
    // 这样装十个整合包也只占一份 Minecraft。
    let dir = versions.join(&name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建实例目录：{e}"))?;
    for sub in [
        "mods",
        "saves",
        "config",
        "logs",
        "resourcepacks",
        "shaderpacks",
        "crash-reports",
    ] {
        let _ = std::fs::create_dir_all(dir.join(sub));
    }
    let mut meta = if pack.loader.is_empty() {
        json!({
            "id": name,
            "inheritsFrom": pack.mc,
            "type": "release",
            "releaseTime": "1970-01-01T00:00:00+00:00",
        })
    } else {
        // 加载器的 profile 自带 mainClass / 库 / 参数，改个名字挂到原版下面即可。
        let mut prof = fetch_loader_profile(h, &pack.loader, &pack.mc, &pack.loader_ver)?;
        prof["id"] = json!(name);
        prof["inheritsFrom"] = json!(pack.mc);
        prof["clientVersion"] = json!(pack.mc);
        prof
    };
    if meta.get("type").is_none() {
        meta["type"] = json!("release");
    }
    let json_path = dir.join(format!("{name}.json"));
    std::fs::write(
        &json_path,
        serde_json::to_vec_pretty(&meta).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("无法写入 {json_path:?}：{e}"))?;
    let dir = dir.to_string_lossy().into_owned();

    // `files[]` 的路径是相对实例目录的（`mods/x.jar`、`config/y.json`），
    // 而 `fetch_all` 的路径是相对游戏根目录的，所以这里补上 `versions/<name>/`。
    let items: Vec<Item> = pack
        .files
        .iter()
        .map(|f| Item {
            path: format!("versions/{name}/{}", f.path),
            url: f.url.clone(),
            size: f.size,
            sha1: f.sha1.clone(),
        })
        .collect();
    let total_bytes: u64 = items.iter().map(|i| i.size).sum();
    let (task, cancel) = task_new(
        "modpack",
        &format!("安装整合包 {name}"),
        items.len() as u64,
    );
    let got = fetch_all(
        h,
        root,
        &items,
        concurrency(h),
        (items.len() / 20).max(1),
        Some(task),
        Some(&cancel),
    );
    if got.cancelled {
        task_cancelled(task, "已取消，已下载的文件保留在实例目录里");
    } else {
        task_finish(task, got.errors.is_empty(), got.errors.first().map(String::as_str).unwrap_or(""));
    }

    // 散文件最后铺：`client-overrides/` 会盖掉 `overrides/` 里的同名文件。
    let overrides = pack::extract_overrides(bytes, &dir).unwrap_or(0);

    unsafe {
        h.emit_str(
            "mc.modpack.installed",
            format!(r#"{{"name":"{name}","mc":"{}"}}"#, pack.mc).as_bytes(),
        );
    }
    Ok(serde_json::to_vec(&json!({
        "ok": got.errors.is_empty() && !got.cancelled,
        "name": name,
        "dir": dir,
        "root": root,
        "mc": pack.mc,
        "loader": pack.loader,
        "loaderVersion": pack.loader_ver,
        "files": items.len(),
        "bytes": total_bytes,
        "downloaded": got.done,
        "overrides": overrides,
        "cancelled": got.cancelled,
        "errors": got.errors,
    }))
    .unwrap_or_default())
}

fn concurrency(h: &MiyHostV1) -> usize {
    call_json(h, "miy.app.settings", &json!({}))
        .ok()
        .and_then(|v| {
            v.get("values")
                .and_then(|x| x.get("miy.mc.concurrency"))
                .and_then(Value::as_u64)
        })
        // 默认 8：镜像节点按 IP 限速，开太猛会被 429 反噬（实测 16 文件 × 8 段会撞上）。
        .unwrap_or(8) as usize
}

// ---- 服务 ----------------------------------------------------------------

/// 已经装好的版本：`<root>/versions/<id>/<id>.json` 存在就算装了。
///
/// 首页和版本页都靠它 —— 看见的必须是磁盘上真有的东西，不是清单里 920 个 id。
/// 从 `libraries[].name` 里猜加载器：返回 (名字, 版本)。
///
/// NeoForge 必须排在 Forge **前面**判：`net.neoforged:neoforge:` 也以
/// `net.` 开头，一旦先撞上 forge 的规则，NeoForge 会被报成 Forge。
fn detect_loader(v: &Value) -> Option<(String, String)> {
    const KNOWN: [(&str, &str); 5] = [
        ("net.fabricmc:fabric-loader:", "Fabric"),
        ("org.quiltmc:quilt-loader:", "Quilt"),
        ("net.neoforged:neoforge:", "NeoForge"),
        ("net.minecraftforge:forge:", "Forge"),
        ("optifine:OptiFine:", "OptiFine"),
    ];
    if let Some(libs) = v.get("libraries").and_then(Value::as_array) {
        for (prefix, name) in KNOWN {
            for l in libs {
                let n = l.get("name").and_then(Value::as_str).unwrap_or("");
                if let Some(rest) = n.strip_prefix(prefix) {
                    let ver = rest.split(':').next().unwrap_or("").to_string();
                    return Some((name.to_string(), ver));
                }
            }
        }
    }
    // 库名认不出来就看主类，至少不会把 Fabric 装成"原版"。
    let mc = v.get("mainClass").and_then(Value::as_str).unwrap_or("");
    let hint = if mc.contains("fabricmc") {
        "Fabric"
    } else if mc.contains("quiltmc") {
        "Quilt"
    } else if mc.contains("neoforge") {
        "NeoForge"
    } else if mc.contains("forge") || mc.contains("bootstraplauncher") {
        "Forge"
    } else {
        return None;
    };
    Some((hint.to_string(), String::new()))
}

fn local_version(dir: &std::path::Path, id: &str) -> Option<Value> {
    let p = dir.join(id).join(format!("{id}.json"));
    let b = std::fs::read(p).ok()?;
    serde_json::from_slice(&b).ok()
}

/// 沿 `inheritsFrom` 往下走，凑出"这个版本到底是什么"：
/// 基版（真正的 MC 版本号）、加载器、Java 版本、发布时间。
fn describe(dir: &std::path::Path, id: &str) -> (String, String, String, u64, String) {
    let mut cur = id.to_string();
    let mut mc = String::new();
    let mut java = 0u64;
    let mut released = String::new();
    let mut loader: Option<(String, String)> = None;

    for _ in 0..6 {
        let Some(v) = local_version(dir, &cur) else { break };
        if loader.is_none() {
            loader = detect_loader(&v);
        }
        if java == 0 {
            java = v
                .get("javaVersion")
                .and_then(|j| j.get("majorVersion"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
        }
        if released.is_empty() {
            released = v
                .get("releaseTime")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        // 合并过的版本 json 里带着 clientVersion（真正的游戏版本），
        // 比按名字猜或顺着 inheritsFrom 往上爬都准。
        if let Some(cv) = v.get("clientVersion").and_then(Value::as_str) {
            let cv = cv.trim();
            if !cv.is_empty() {
                mc = cv.to_string();
                break;
            }
        }
        match v.get("inheritsFrom").and_then(Value::as_str) {
            Some(parent) => cur = parent.to_string(),
            None => {
                mc = cur.clone();
                break;
            }
        }
    }
    if mc.is_empty() {
        mc = id.to_string();
    }
    let (loader_name, loader_ver) = loader.unwrap_or_else(|| ("原版".to_string(), String::new()));
    (mc, loader_name, loader_ver, java, released)
}

fn service_installed(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    // 请求里指定了 root 就只看那一个，否则把所有登记的 .minecraft 都扫一遍。
    let roots: Vec<String> = match field(req, "root") {
        Some(r) => vec![r.to_string()],
        None => all_roots(h),
    };
    let mut list: Vec<Value> = Vec::new();
    let mut broken = 0u32;
    for root in &roots {
    let dir = std::path::Path::new(root).join("versions");
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            let json_file = entry.path().join(format!("{id}.json"));
            if !json_file.is_file() {
                continue;
            }
            let jar = entry.path().join(format!("{id}.jar"));
            let jar_size = std::fs::metadata(&jar).map(|m| m.len()).unwrap_or(0);
            // 上次没装完的版本：标记还在就别当成能用的版本。
            let marker = entry.path().join(".incomplete");
            let incomplete = marker.is_file();
            let kind = std::fs::read(&json_file)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| "release".to_string());
            let (mc, loader, loader_ver, java, released) = describe(&dir, &id);
            // 加载器版本通常没有自己的 jar，真正的客户端 jar 在基版目录里。
            let base_jar = dir.join(&mc).join(format!("{mc}.jar"));
            let runnable = jar_size > 0 || base_jar.is_file();
            let ok = runnable && !incomplete;
            if !ok && !incomplete {
                broken += 1;
            }
            list.push(json!({
                "id": id,
                "type": kind,
                "mc": mc,
                "loader": loader,
                "loaderVer": loader_ver,
                "java": java,
                "released": released,
                "jar": jar_size,
                "runnable": runnable,
                "incomplete": incomplete,
                "ok": ok,
                "root": root,
                "rootName": std::path::Path::new(root)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| root.clone()),
            }));
        }
    }
    }
    // 能用的排前面，其次按名字倒序（版本号里常带日期）。
    list.sort_by(|a, b| {
        let oa = a["ok"].as_bool().unwrap_or(false);
        let ob = b["ok"].as_bool().unwrap_or(false);
        ob.cmp(&oa).then_with(|| {
            b["id"]
                .as_str()
                .unwrap_or("")
                .cmp(a["id"].as_str().unwrap_or(""))
        })
    });
    Ok(json!({
        "ok": true,
        "root": roots.first().cloned().unwrap_or_default(),
        "roots": roots,
        "versions": list,
        "broken": broken,
    })
    .to_string()
    .into_bytes())
}

/// 删掉一个装坏的版本目录。
///
/// 只允许删 `<root>/versions/<name>` 这一层：名字里带分隔符或 `..` 的一律拒绝，
/// 否则一个请求就能删掉任意目录。
fn service_remove(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let root = game_root(h, req);
    let name = field(req, "name").unwrap_or("").trim().to_string();
    if name.is_empty() {
        return Err("缺少版本名".into());
    }
    if name.contains('\\')
        || name.contains('/')
        || name.contains("..")
        || name.contains(':')
        || name.contains('*')
        || name.contains('?')
    {
        return Err("版本名不合法".into());
    }
    let dir = std::path::Path::new(&root).join("versions").join(&name);
    if !dir.is_dir() {
        return Err(format!("没有这个版本：{name}"));
    }
    std::fs::remove_dir_all(&dir).map_err(|e| format!("无法删除 {name}：{e}"))?;
    unsafe {
        h.emit_str(
            "mc.removed",
            format!(r#"{{"name":"{name}"}}"#).as_bytes(),
        )
    };
    Ok(json!({ "ok": true, "name": name, "root": root })
        .to_string()
        .into_bytes())
}

/// 版本目录改名：目录名和里面的 `<名字>.json` 一起改。
///
/// 目录名就是"这个实例叫什么"，json 的文件名必须跟目录名一致 ——
/// `describe` / `resolve_local` / 启动都按 `<root>/versions/<name>/<name>.json` 找。
fn service_rename(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let root = game_root(h, req);
    let from = field(req, "from").unwrap_or("").trim().to_string();
    let to = field(req, "to").unwrap_or("").trim().to_string();
    if from.is_empty() || to.is_empty() {
        return Err("缺少版本名".into());
    }
    let bad = |s: &str| {
        s.contains('\\')
            || s.contains('/')
            || s.contains("..")
            || s.contains(':')
            || s.contains('*')
            || s.contains('?')
            || s.contains('"')
            || s.contains('<')
            || s.contains('>')
            || s.contains('|')
    };
    if bad(&from) || bad(&to) {
        return Err("版本名不合法".into());
    }
    if from == to {
        return Ok(json!({ "ok": true, "from": from, "to": to, "changed": false })
            .to_string()
            .into_bytes());
    }
    let versions = std::path::Path::new(&root).join("versions");
    let src = versions.join(&from);
    let dst = versions.join(&to);
    if !src.is_dir() {
        return Err(format!("没有这个版本：{from}"));
    }
    if dst.exists() {
        return Err(format!("已经有同名版本：{to}"));
    }
    std::fs::rename(&src, &dst).map_err(|e| format!("改名失败：{e}"))?;
    let old = dst.join(format!("{from}.json"));
    let new = dst.join(format!("{to}.json"));
    if old.is_file() {
        let _ = std::fs::rename(&old, &new);
    }
    unsafe {
        h.emit_str(
            "mc.renamed",
            format!(r#"{{"from":"{from}","to":"{to}"}}"#).as_bytes(),
        )
    };
    Ok(json!({ "ok": true, "from": from, "to": to, "changed": true, "dir": dst.display().to_string() })
        .to_string()
        .into_bytes())
}

/// 强制刷新版本清单：跳过缓存，镜像 → 官方，成功后落盘并发事件。
///
/// 版本页的「刷新」按钮用它 —— 快照版发布后不用等缓存过期，
/// 也不用重启启动器。
fn service_manifest_refresh(h: &MiyHostV1) -> Result<Vec<u8>, String> {
    let started = std::time::Instant::now();
    if let Ok(mut g) = MAN_CACHE.lock() {
        *g = None;
    }
    let mut last = String::from("没有可用的清单地址");
    let mut got: Option<(Value, &'static str)> = None;
    for url in manifest_urls(h) {
        match fetch_json(h, url) {
            Ok(v) => {
                got = Some((v, url));
                break;
            }
            Err(e) => last = format!("{url}: {e}"),
        }
    }
    let Some((v, url)) = got else {
        return Err(last);
    };
    if let Ok(mut g) = MAN_CACHE.lock() {
        *g = Some((std::time::Instant::now(), v.clone()));
    }
    save_manifest_disk(h, &v);
    unsafe { h.emit_str("mc.versions.updated", b"{}") };
    let latest = v.get("latest").cloned().unwrap_or(Value::Null);
    let count = v
        .get("versions")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    Ok(serde_json::to_vec(&json!({
        "ok": true,
        "latest": latest,
        "count": count,
        "source": url,
        "ms": started.elapsed().as_millis() as u64,
    }))
    .unwrap_or_default())
}

fn service_versions(h: &MiyHostV1) -> Result<Vec<u8>, String> {
    // 先给能立刻拿到的：内存缓存 → 落盘清单 → 才去联网。
    // 缓存超过一分钟就顺手在后台换一份新的，换完发事件让界面自己重画。
    // 阈值压到 60 秒：快照版发布后一分钟内就能在列表里看到。
    let stale = man_age()
        .map(|a| a > std::time::Duration::from_secs(60))
        .unwrap_or(true);
    let man = match man_cached() {
        Some(v) => {
            if stale {
                refresh_manifest_bg(h);
            }
            v
        }
        None => match load_manifest_disk(h) {
            Some(v) => {
                if let Ok(mut g) = MAN_CACHE.lock() {
                    *g = Some((std::time::Instant::now(), v.clone()));
                }
                refresh_manifest_bg(h);
                v
            }
            None => manifest(h)?,
        },
    };
    // 愚人节版本在清单里的 type 也是 snapshot，只能按 id 认。名单是官方出过的那几个。
    const APRIL: [&str; 9] = [
        "2.0",
        "15w14a",
        "1.RV-Pre1",
        "3D Shareware v1.34",
        "20w14infinite",
        "22w13oneblockatatime",
        "23w13a_or_b",
        "24w14potato",
        "25w14craftmine",
    ];
    let versions: Vec<Value> = man
        .get("versions")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|v| {
                    let id = v.get("id").and_then(Value::as_str).unwrap_or("");
                    let april = APRIL.iter().any(|a| a.eq_ignore_ascii_case(id));
                    json!({
                        "id": v.get("id").cloned().unwrap_or(Value::Null),
                        "type": v.get("type").cloned().unwrap_or(Value::Null),
                        "releaseTime": v.get("releaseTime").cloned().unwrap_or(Value::Null),
                        "april": april,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(serde_json::to_vec(&json!({
        "latest": man.get("latest").cloned().unwrap_or(Value::Null),
        "count": man.get("versions").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
        "versions": versions,
    }))
    .unwrap_or_default())
}

/// 只读本地版本 JSON 的轻量信息：不碰网络清单，毫秒级。
/// 模组列表页只要 mc 版本和加载器，没必要为它去拉一遍版本清单。
fn service_verinfo(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let id = field(req, "id").ok_or("缺 id")?;
    let root = game_root(h, req);
    let dir = std::path::Path::new(&root).join("versions").join(&id);
    let file = dir.join(format!("{id}.json"));
    let v: Value = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| format!("找不到版本 {id} 的配置文件"))?;
    let base = v
        .get("inheritsFrom")
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_string();
    let lower = id.to_lowercase();
    let loader = if lower.contains("neoforge") {
        "neoforge"
    } else if lower.contains("fabric") {
        "fabric"
    } else if lower.contains("quilt") {
        "quilt"
    } else if lower.contains("forge") {
        "forge"
    } else {
        ""
    };
    Ok(serde_json::to_vec(&json!({
        "ok": true,
        "id": id,
        "mc": base,
        "loader": loader,
        "type": v.get("type").cloned().unwrap_or(Value::Null),
        "mainClass": v.get("mainClass").cloned().unwrap_or(Value::Null),
    }))
    .unwrap_or_default())
}

fn service_info(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let id = field(req, "id").ok_or("缺 id")?;
    let man = manifest_for(h, id);
    let v = resolve(h, &man, id, 0)?;
    let (libs, extra) = plan(&v, id);
    let total: u64 = libs.iter().chain(extra.iter()).map(|i| i.size).sum();
    // 模组更新要按「这个版本实际对应的 MC 版本 + 加载器」去查，所以一并返回。
    let base = v
        .get("inheritsFrom")
        .and_then(Value::as_str)
        .unwrap_or(id)
        .to_string();
    let lower = id.to_lowercase();
    let loader = if lower.contains("neoforge") {
        "neoforge"
    } else if lower.contains("fabric") {
        "fabric"
    } else if lower.contains("quilt") {
        "quilt"
    } else if lower.contains("forge") {
        "forge"
    } else {
        ""
    };
    let summary = json!({
        "id": v.get("id").cloned().unwrap_or(Value::Null),
        "mc": base,
        "loader": loader,
        "type": v.get("type").cloned().unwrap_or(Value::Null),
        "mainClass": v.get("mainClass").cloned().unwrap_or(Value::Null),
        "assets": v.get("assets").cloned().unwrap_or(Value::Null),
        "javaVersion": v.get("javaVersion").cloned().unwrap_or(Value::Null),
        "inheritsFrom": v.get("inheritsFrom").cloned().unwrap_or(Value::Null),
        "libraries": libs.len(),
        "extra": extra.len(),
        "files": libs.len() + extra.len(),
        "totalBytes": total,
        "root": game_root(h, req),
    });
    Ok(serde_json::to_vec(&summary).unwrap_or_default())
}

/// 可用的模组加载器（Fabric / Quilt 走各自的 meta 接口，纯 JSON，不需要跑安装器）。
const FABRIC_META: &str = "https://meta.fabricmc.net/v2";
const QUILT_META: &str = "https://meta.quiltmc.org/v3";

/// 加载器清单的磁盘缓存位置（跨重启有效）。
fn loaders_cache_file(h: &MiyHostV1, mc: &str) -> Option<std::path::PathBuf> {
    let info = call_json(h, "miy.app.info", &json!({})).ok()?;
    let data = info.get("dataRoot").and_then(Value::as_str)?;
    let safe: String = mc
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    Some(
        std::path::Path::new(data)
            .join("cache")
            .join(format!("loaders-v2-{safe}.json")),
    )
}

/// 拉一个加载器的版本列表：先走自建中转（按天缓存），失败再直连官方。
fn loader_meta(h: &MiyHostV1, kind: &str, official: &str, mc: &str) -> Option<Vec<Value>> {
    let path = format!("/versions/loader/{mc}");
    let mut urls: Vec<String> = Vec::new();
    if !site_dead() {
        urls.push(format!("{}/{kind}{path}", site_base()));
    }
    urls.push(format!("{official}{path}"));
    for u in urls {
        let Ok(v) = fetch_json(h, &u) else { continue };
        let list: Vec<Value> = v
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|it| {
                        let l = it.get("loader")?;
                        Some(json!({
                            "version": l.get("version").cloned().unwrap_or(Value::Null),
                            "stable": l.get("stable").cloned().unwrap_or(Value::Bool(false)),
                        }))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !list.is_empty() {
            return Some(list);
        }
    }
    None
}

/// 从 `maven-metadata.xml` 里抠出所有 `<version>`（maven 按升序排）。
fn maven_versions(h: &MiyHostV1, urls: &[String]) -> Option<Vec<String>> {
    for u in urls {
        let Ok(text) = fetch_text(h, u) else { continue };
        let mut out = Vec::new();
        let mut rest = text.as_str();
        while let Some(i) = rest.find("<version>") {
            let after = &rest[i + "<version>".len()..];
            let Some(j) = after.find("</version>") else { break };
            let v = after[..j].trim();
            if !v.is_empty() {
                out.push(v.to_string());
            }
            rest = &after[j + "</version>".len()..];
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

/// Forge 的版本号是 `<mc>-<forge>`，例如 `26.3-66.1.5`。
fn forge_meta(h: &MiyHostV1, mc: &str) -> Option<Vec<Value>> {
    let urls = vec![
        format!("{}/maven/net/minecraftforge/forge/maven-metadata.xml", mirror_base()),
        "https://maven.minecraftforge.net/net/minecraftforge/forge/maven-metadata.xml".to_string(),
    ];
    let all = maven_versions(h, &urls)?;
    let prefix = format!("{mc}-");
    let mut list: Vec<Value> = all
        .iter()
        .filter_map(|v| v.strip_prefix(prefix.as_str()))
        .map(|v| json!({"version": v, "stable": true}))
        .collect();
    // maven 是升序，界面上新的要排前面。
    list.reverse();
    if list.is_empty() {
        None
    } else {
        Some(list)
    }
}

/// NeoForge 的版本号跟着 MC 版本走：`26.1.2` → `26.1.2.125`；
/// 老版本是 `1.21.1` → `21.1.80`（去掉开头的 `1.`）。
fn neoforge_meta(h: &MiyHostV1, mc: &str) -> Option<Vec<Value>> {
    let urls = vec![
        format!("{}/maven/net/neoforged/neoforge/maven-metadata.xml", mirror_base()),
        "https://maven.neoforged.net/releases/net/neoforged/neoforge/maven-metadata.xml".to_string(),
    ];
    let all = maven_versions(h, &urls)?;
    let mut prefixes = vec![format!("{mc}.")];
    if let Some(rest) = mc.strip_prefix("1.") {
        prefixes.push(format!("{rest}."));
    }
    let mut list: Vec<Value> = Vec::new();
    for p in &prefixes {
        list = all
            .iter()
            .filter(|v| v.starts_with(p.as_str()))
            .map(|v| json!({"version": v, "stable": true}))
            .collect();
        if !list.is_empty() {
            break;
        }
    }
    list.reverse();
    if list.is_empty() {
        None
    } else {
        Some(list)
    }
}

fn service_loaders(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let mc = field(req, "mc").ok_or("缺 mc")?;
    // 加载器列表一天也不会变：内存缓存 10 分钟 + 磁盘缓存（跨重启）。
    // 官方 meta 在国外，冷查要 4~5 秒、响应 600~900 KB；自建中转按天缓存，
    // 所以先问它，拿不到再直连官方。
    static CACHE: Mutex<Option<(String, std::time::Instant, Value)>> = Mutex::new(None);
    if let Ok(g) = CACHE.lock() {
        if let Some((k, at, v)) = g.as_ref() {
            if k == mc && at.elapsed().as_secs() < 600 {
                return Ok(serde_json::to_vec(v).unwrap_or_default());
            }
        }
    }
    let disk = loaders_cache_file(h, mc);
    if let Some(v) = disk
        .as_ref()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    {
        if v.get("mc").and_then(Value::as_str) == Some(mc) {
            if let Ok(mut g) = CACHE.lock() {
                *g = Some((mc.to_string(), std::time::Instant::now(), v.clone()));
            }
            return Ok(serde_json::to_vec(&v).unwrap_or_default());
        }
    }

    // 四个源并行拉：串行时每个 4 秒叠起来就是十几秒。
    let cell = HostCell(*h);
    let mc1 = mc.to_string();
    let t1 = std::thread::spawn(move || loader_meta(cell.h(), "fabric-meta", FABRIC_META, &mc1));
    let cell2 = HostCell(*h);
    let mc2 = mc.to_string();
    let t2 = std::thread::spawn(move || loader_meta(cell2.h(), "quilt-meta", QUILT_META, &mc2));
    let cell3 = HostCell(*h);
    let mc3 = mc.to_string();
    let t3 = std::thread::spawn(move || forge_meta(cell3.h(), &mc3));
    let cell4 = HostCell(*h);
    let mc4 = mc.to_string();
    let t4 = std::thread::spawn(move || neoforge_meta(cell4.h(), &mc4));
    let fabric = t1.join().ok().flatten();
    let quilt = t2.join().ok().flatten();
    let forge = t3.join().ok().flatten();
    let neoforge = t4.join().ok().flatten();

    let mut out = Vec::new();
    if let Some(list) = fabric {
        out.push(json!({"id":"fabric","name":"Fabric","versions":list}));
    }
    if let Some(list) = quilt {
        out.push(json!({"id":"quilt","name":"Quilt","versions":list}));
    }
    if let Some(list) = forge {
        out.push(json!({"id":"forge","name":"Forge","versions":list}));
    }
    if let Some(list) = neoforge {
        out.push(json!({"id":"neoforge","name":"NeoForge","versions":list}));
    }
    let body = json!({"ok": true, "mc": mc, "loaders": out});
    if let Ok(mut g) = CACHE.lock() {
        *g = Some((mc.to_string(), std::time::Instant::now(), body.clone()));
    }
    if let Some(p) = disk {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(b) = serde_json::to_vec(&body) {
            let _ = std::fs::write(&p, b);
        }
    }
    Ok(serde_json::to_vec(&body).unwrap_or_default())
}

/// 拉加载器的 profile JSON。Fabric / Quilt 的 meta 都是纯 JSON，不用跑安装器。
fn fetch_loader_profile(h: &MiyHostV1, loader: &str, mc: &str, lv: &str) -> Result<Value, String> {
    let url = match loader {
        "fabric" => format!("{FABRIC_META}/versions/loader/{mc}/{lv}/profile/json"),
        "quilt" => format!("{QUILT_META}/versions/loader/{mc}/{lv}/profile/json"),
        other => return Err(format!("暂不支持这个加载器：{other}")),
    };
    fetch_json(h, &url).map_err(|e| format!("取加载器配置失败：{e}"))
}

/// 装一个加载器：拉它的 profile JSON，改个名字写进 `versions/<name>/<name>.json`。
///
/// 加载器的 profile 里 `inheritsFrom` 指向原版，启动时沿这条链合并，
/// 所以不需要再复制一份原版文件。
fn service_loader_install(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let mc = field(req, "mc").ok_or("缺 mc")?;
    let loader = field(req, "loader").ok_or("缺 loader")?;
    let lv = field(req, "loaderVersion").ok_or("缺 loaderVersion")?;
    let name = field(req, "name")
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("{mc}-{loader}"));
    if name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
        return Err(format!("版本名里不能有特殊字符：{name}"));
    }
    // 加载器要跟它的原版待在同一个目录里：库和资源都在那边，
    // 装到别的目录会变成「缺 74 个运行库」。
    let root = field(req, "root")
        .map(str::to_string)
        .or_else(|| root_of_version(h, mc))
        .unwrap_or_else(|| game_root(h, req));
    let mut v = fetch_loader_profile(h, &loader, mc, lv)?;
    if let Some(o) = v.as_object_mut() {
        o.insert("id".into(), Value::String(name.clone()));
        o.insert("inheritsFrom".into(), Value::String(mc.to_string()));
    }
    let dir = format!("{root}\\versions\\{name}");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败：{dir}（{e}）"))?;
    for sub in ["mods", "saves", "config", "logs", "resourcepacks", "shaderpacks", "crash-reports"] {
        let _ = std::fs::create_dir_all(format!("{dir}\\{sub}"));
    }
    let file = format!("{dir}\\{name}.json");
    std::fs::write(&file, serde_json::to_vec_pretty(&v).map_err(|e| e.to_string())?)
        .map_err(|e| format!("无法写入 {file}：{e}"))?;
    // 装完 Fabric 顺手把 Fabric API 放进 mods：绝大多数 Fabric 模组缺它起不来，
    // 用户装完加载器第一件事就是找它。失败不影响加载器本身。
    let mut fabric_api = String::new();
    let mut fabric_api_error = String::new();
    if loader == "fabric" {
        match install_fabric_api(h, &dir, mc) {
            Ok(f) => fabric_api = f,
            Err(e) => fabric_api_error = e,
        }
    }
    Ok(serde_json::to_vec(&json!({
        "ok": true, "name": name, "dir": dir, "loader": loader, "loaderVersion": lv,
        "fabricApi": fabric_api, "fabricApiError": fabric_api_error,
    }))
    .unwrap_or_default())
}

/// 跑加载器官方安装器（Forge / NeoForge），带超时与取消。
///
/// 这两个加载器不是「一份 profile JSON」就能装好的：它们要拿原版 jar 做二进制
/// 补丁、生成 client-extra、再跑一串 processor。官方 installer 把这些都做完了，
/// 自己实现等于重写一遍 Forge 的安装器，所以这里直接调它。
fn run_installer(
    java: &str,
    jar: &str,
    root: &str,
    cancel: &Arc<AtomicBool>,
    timeout: std::time::Duration,
) -> Result<String, String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut child = Command::new(java)
        .arg("-jar")
        .arg(jar)
        .arg("--installClient")
        .arg(root)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("启动安装器失败：{e}"))?;

    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_buf = Arc::new(Mutex::new(String::new()));
    let err_buf = Arc::new(Mutex::new(String::new()));
    let o = out_buf.clone();
    let t_out = std::thread::spawn(move || {
        if let Some(p) = out_pipe.as_mut() {
            let mut s = String::new();
            let _ = p.read_to_string(&mut s);
            if let Ok(mut g) = o.lock() {
                g.push_str(&s);
            }
        }
    });
    let e = err_buf.clone();
    let t_err = std::thread::spawn(move || {
        if let Some(p) = err_pipe.as_mut() {
            let mut s = String::new();
            let _ = p.read_to_string(&mut s);
            if let Ok(mut g) = e.lock() {
                g.push_str(&s);
            }
        }
    });

    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = t_out.join();
                let _ = t_err.join();
                let out = out_buf.lock().map(|g| g.clone()).unwrap_or_default();
                let err = err_buf.lock().map(|g| g.clone()).unwrap_or_default();
                if status.success() {
                    return Ok(out);
                }
                // 安装器的报错通常只有最后几行有用，前面全是下载进度。
                let src = if err.trim().is_empty() { &out } else { &err };
                let tail: Vec<&str> = src.lines().rev().take(6).collect();
                let tail: Vec<&str> = tail.into_iter().rev().collect();
                return Err(format!(
                    "安装器退出码 {:?}：{}",
                    status.code(),
                    tail.join(" / ")
                ));
            }
            Ok(None) => {}
            Err(e) => return Err(format!("等待安装器失败：{e}")),
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("已取消".to_string());
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err("安装器超时（15 分钟）".to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// 把安装器要用的库先铺进 `<root>/libraries`。
///
/// 安装器只会从它自己的 maven 拉库，而 `maven.neoforged.net` 在国内经常连不上
/// （实测直接超时）。安装器发现文件已经在 `libraries` 里就会跳过下载，
/// 所以我们先用镜像铺一遍，再让它跑。
fn seed_installer_libs(
    h: &MiyHostV1,
    root: &str,
    jar: &str,
    task: u64,
    cancel: &Arc<AtomicBool>,
) -> Result<usize, String> {
    let bytes = std::fs::read(jar).map_err(|e| format!("读安装器失败：{e}"))?;
    let entries = zip_list(&bytes)?;
    let mut items: Vec<Item> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for name in ["version.json", "install_profile.json"] {
        let Some(e) = entries.iter().find(|e| e.name == name) else {
            continue;
        };
        let Ok(raw) = zip_read(&bytes, e) else { continue };
        let Ok(v) = serde_json::from_slice::<Value>(&raw) else {
            continue;
        };
        let Some(libs) = v.get("libraries").and_then(Value::as_array) else {
            continue;
        };
        for l in libs {
            let art = l.get("downloads").and_then(|d| d.get("artifact"));
            let path = art
                .and_then(|a| a.get("path"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| l.get("name").and_then(Value::as_str).and_then(maven_rel));
            let Some(path) = path else { continue };
            if !seen.insert(path.clone()) {
                continue;
            }
            let sha1 = art
                .and_then(|a| a.get("sha1"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let size = art
                .and_then(|a| a.get("size"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            items.push(Item {
                path: format!("libraries/{path}"),
                url: format!("{}/maven/{path}", mirror_base()),
                size,
                sha1,
            });
        }
    }
    if items.is_empty() {
        return Ok(0);
    }
    let n = items.len();
    let got = fetch_all(h, root, &items, concurrency(h), 0, Some(task), Some(cancel));
    if got.cancelled {
        return Err("已取消".to_string());
    }
    // 并发下漏掉的文件（限流、超时）串行补一遍：再并发只会继续撞限流。
    let missing: Vec<Item> = items
        .iter()
        .filter(|it| !std::path::Path::new(&format!("{root}/{}", it.path)).is_file())
        .cloned()
        .collect();
    if !missing.is_empty() {
        let again = fetch_all(h, root, &missing, 1, 0, Some(task), Some(cancel));
        if again.cancelled {
            return Err("已取消".to_string());
        }
    }
    let left = items
        .iter()
        .filter(|it| !std::path::Path::new(&format!("{root}/{}", it.path)).is_file())
        .count();
    if left > 0 {
        return Err(format!("还有 {left} 个安装器依赖没下下来"));
    }
    Ok(n)
}

/// 装 Forge / NeoForge：先保证原版在，再跑官方安装器。
fn service_install_forge(
    h: &MiyHostV1,
    req: &Value,
    mc: &str,
    loader: &str,
    ver: &str,
) -> Result<Vec<u8>, String> {
    let root = game_root(h, req);
    let base_json = format!("{root}\\versions\\{mc}\\{mc}.json");
    let base_jar = format!("{root}\\versions\\{mc}\\{mc}.jar");
    let mut notes: Vec<String> = Vec::new();
    if !std::path::Path::new(&base_json).is_file() || !std::path::Path::new(&base_jar).is_file() {
        // 安装器要读原版的 json 和客户端 jar 来做补丁，缺一个都装不了。
        let mut r = req.clone();
        if let Some(o) = r.as_object_mut() {
            o.insert("loader".into(), json!(""));
            o.insert("loaderVersion".into(), json!(""));
            o.insert("name".into(), json!(mc));
        }
        service_install(h, &r)?;
        notes.push(format!("先装好了原版 {mc}"));
    }

    let (file, url) = if loader == "forge" {
        (
            format!("forge-{mc}-{ver}-installer.jar"),
            format!(
                "{}/maven/net/minecraftforge/forge/{mc}-{ver}/forge-{mc}-{ver}-installer.jar",
                mirror_base()
            ),
        )
    } else {
        (
            format!("neoforge-{ver}-installer.jar"),
            format!(
                "{}/maven/net/neoforged/neoforge/{ver}/neoforge-{ver}-installer.jar",
                mirror_base()
            ),
        )
    };
    let cache = format!("{root}\\cache\\installers");
    std::fs::create_dir_all(&cache).map_err(|e| format!("创建缓存目录失败：{e}"))?;
    let jar = format!("{cache}\\{file}");
    let (task, cancel) = task_new("install", &format!("安装 {loader} {ver}"), 3);
    if !std::path::Path::new(&jar).is_file() {
        unsafe { h.log_str(MIY_LOG_INFO, &format!("正在下载 {loader} 安装器…")) };
        if let Err(e) = download(h, &url, &jar, 0) {
            task_finish(task, false, &e);
            return Err(format!("下载安装器失败：{e}"));
        }
    }
    task_progress(task, 1, 0);

    // 安装器自己的 maven 在国内经常连不上，先用镜像把库铺好。
    match seed_installer_libs(h, &root, &jar, task, &cancel) {
        Ok(n) if n > 0 => unsafe {
            h.log_str(
                MIY_LOG_INFO,
                &format!("已用镜像铺好 {n} 个安装器依赖"),
            )
        },
        Ok(_) => {}
        Err(e) if e == "已取消" => {
            task_cancelled(task, "已取消");
            return Ok(
                serde_json::to_vec(&json!({"ok": false, "cancelled": true})).unwrap_or_default(),
            );
        }
        Err(e) => unsafe {
            h.log_str(
                MIY_LOG_WARN,
                &format!("预铺安装器依赖失败（继续尝试）：{e}"),
            )
        },
    }

    // 安装器要求根目录里有 launcher_profiles.json，否则直接报错退出。
    let profiles = format!("{root}\\launcher_profiles.json");
    if !std::path::Path::new(&profiles).is_file() {
        let _ = std::fs::write(&profiles, br#"{"profiles":{},"settings":{},"version":3}"#);
    }

    let man = manifest_for(h, mc);
    let v = resolve(h, &man, mc, 0)?;
    let (need, comp) = java::required(&v, mc);
    let j = match java::ensure(h, need, comp.as_deref(), true, &mut notes) {
        Ok(j) => j,
        Err(e) => {
            task_finish(task, false, &e);
            return Err(e);
        }
    };
    unsafe { h.log_str(MIY_LOG_INFO, &format!("正在运行 {loader} 安装器（Java {}）…", j.major)) };
    let out = match run_installer(
        &j.path,
        &jar,
        &root,
        &cancel,
        std::time::Duration::from_secs(900),
    ) {
        Ok(o) => o,
        Err(e) => {
            if e == "已取消" {
                task_cancelled(task, "已取消");
                return Ok(
                    serde_json::to_vec(&json!({"ok": false, "cancelled": true})).unwrap_or_default(),
                );
            }
            task_finish(task, false, &e);
            return Err(e);
        }
    };
    task_progress(task, 2, 0);

    let name = if loader == "forge" {
        format!("{mc}-forge-{ver}")
    } else {
        format!("neoforge-{ver}")
    };
    let dir = format!("{root}\\versions\\{name}");
    let json_path = format!("{dir}\\{name}.json");
    if !std::path::Path::new(&json_path).is_file() {
        let tail: Vec<&str> = out.lines().rev().take(3).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        let msg = format!(
            "安装器跑完了，但没找到 {json_path}。安装器输出：{}",
            tail.join(" / ")
        );
        task_finish(task, false, &msg);
        return Err(msg);
    }
    task_progress(task, 3, 0);
    task_finish(task, true, "");
    Ok(serde_json::to_vec(&json!({
        "ok": true, "id": name, "name": name, "dir": dir, "root": root,
        "loader": loader, "loaderVersion": ver, "files": 0, "bytes": 0,
        "notes": notes,
    }))
    .unwrap_or_default())
}

/// 找客户端 jar：先看自己目录，没有就沿 `inheritsFrom` 往上找。
///
/// 加载器版本（Fabric / Quilt）的 profile 里没有 jar，游戏本体还是原版那份；
/// 直接按 `<id>.jar` 找会误报「缺少客户端文件」。
fn client_jar_for(roots: &[String], id: &str, version: &Value) -> Option<String> {
    for r in roots {
        let own = format!("{r}\\versions\\{id}\\{id}.jar");
        if std::path::Path::new(&own).is_file() {
            return Some(own);
        }
    }
    let mut cur = version.clone();
    // 合并后的 json 不一定还留着 inheritsFrom，直接读盘上的原始文件更可靠。
    for r in roots {
        let p = format!("{r}\\versions\\{id}\\{id}.json");
        if let Some(t) = read_text(&p) {
            if let Ok(v) = serde_json::from_str::<Value>(&t) {
                cur = v;
                break;
            }
        }
    }
    for _ in 0..6 {
        let parent = cur.get("inheritsFrom").and_then(Value::as_str)?.to_string();
        for r in roots {
            let jar = format!("{r}\\versions\\{parent}\\{parent}.jar");
            if std::path::Path::new(&jar).is_file() {
                return Some(jar);
            }
        }
        let mut next: Option<Value> = None;
        for r in roots {
            let json = format!("{r}\\versions\\{parent}\\{parent}.json");
            if let Some(t) = read_text(&json) {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    next = Some(v);
                    break;
                }
            }
        }
        cur = next?;
    }
    None
}

/// 这个版本是不是 Fabric 加载器版本（看主类与依赖里有没有 fabricmc）。
fn is_fabric_version(v: &Value) -> bool {
    if let Some(mc) = v.get("mainClass").and_then(Value::as_str) {
        if mc.to_ascii_lowercase().contains("fabric") {
            return true;
        }
    }
    v.get("libraries")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter().any(|l| {
                l.get("name")
                    .and_then(Value::as_str)
                    .map(|n| n.starts_with("net.fabricmc"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// 装完 Fabric 版本后顺手把 Fabric API 放进 mods：没有它，绝大多数 Fabric 模组
/// 一启动就报缺依赖（PCL2 也会提示装它）。
fn install_fabric_api(h: &MiyHostV1, dir: &str, mc: &str) -> Result<String, String> {
    let modrinth = modrinth_base();
    let url = format!(
        "{modrinth}/project/fabric-api/version?game_versions={}&loaders={}",
        enc(&format!("[\"{mc}\"]")),
        enc("[\"fabric\"]")
    );
    let v = fetch_json(h, &url)?;
    let first = v
        .as_array()
        .and_then(|a| a.first())
        .ok_or("没有找到适用于该版本的 Fabric API")?;
    let files = first
        .get("files")
        .and_then(Value::as_array)
        .ok_or("Fabric API 版本缺少文件")?;
    let file = files
        .iter()
        .find(|f| f.get("primary").and_then(Value::as_bool).unwrap_or(false))
        .or_else(|| files.first())
        .ok_or("Fabric API 版本缺少文件")?;
    let fname = file
        .get("filename")
        .and_then(Value::as_str)
        .ok_or("Fabric API 文件没有名字")?;
    let furl = file
        .get("url")
        .and_then(Value::as_str)
        .ok_or("Fabric API 文件没有地址")?;
    let size = file.get("size").and_then(Value::as_u64).unwrap_or(0);
    let mods = format!("{dir}\\mods");
    std::fs::create_dir_all(&mods).map_err(|e| format!("创建 mods 目录失败：{e}"))?;
    // 已经有 Fabric API 就别再下一份：同一个 mods 里放两个版本，Fabric 会报重复模组。
    if let Ok(rd) = std::fs::read_dir(&mods) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_lowercase();
            if n.starts_with("fabric-api-") && n.ends_with(".jar") {
                return Ok(e.file_name().to_string_lossy().to_string());
            }
        }
    }
    let path = format!("{mods}\\{fname}");
    if std::path::Path::new(&path).is_file() {
        return Ok(fname.to_string());
    }
    download(h, furl, &path, size)?;
    Ok(fname.to_string())
}

fn service_install(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let id = field(req, "id").ok_or("缺 id")?;
    // 选了加载器就**一次装成一个版本**：把加载器的 profile 合进原版，
    // 客户端 jar、运行库、Fabric API 全落在同一个目录里。
    // 以前是「先装原版、再装加载器」，一次安装会在版本页冒出两条同名版本，
    // 而且两条各缺一半东西（原版没有 Fabric API，加载器版没有客户端 jar）。
    let loader = field(req, "loader").unwrap_or("").to_string();
    let loader_ver = field(req, "loaderVersion").unwrap_or("").to_string();
    let merged = !loader.is_empty() && !loader_ver.is_empty();
    // Forge / NeoForge 没有「一份 profile JSON」可合并：它们要跑官方安装器
    // 给原版 jar 打补丁，所以走另一条路（目录名也由安装器决定）。
    if merged && (loader == "forge" || loader == "neoforge") {
        return service_install_forge(h, req, id, &loader, &loader_ver);
    }
    // 版本目录名：默认就是版本 id（带加载器时是 `26.1.2-fabric`）。目录名即用户看到的名字。
    let name = field(req, "name")
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if merged {
                format!("{id}-{loader}")
            } else {
                id.to_string()
            }
        });
    if name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
        return Err(format!("版本名里不能有特殊字符：{name}"));
    }
    let root = game_root(h, req);
    let man = manifest_for(h, id);
    let mut v = resolve(h, &man, id, 0)?;
    if merged {
        // 本地 json 可能已经是「合并过加载器」的那份：拿它当底再合并一次，
        // 等于把加载器的库又叠一遍，而且原版那份 natives 早就被去重掉了。
        // 这种情况回网络取原版 json 当底。
        if is_merged_loader(&v) {
            if let Ok(fresh) = fetch_vanilla(h, &man, id) {
                v = fresh;
            }
        }
        let prof = fetch_loader_profile(h, &loader, id, &loader_ver)?;
        // 合并后 **不要** 改 json 里的 `id`：`plan()` 要用它拼客户端地址
        // `{mirror}/version/<id>/client`，改成目录名会拼出不存在的 404 地址。
        v = merge_loader(&v, &prof, id);
    }
    let (libs, extra) = plan(&v, &name);
    let started = std::time::Instant::now();

    let dir = format!("{root}\\versions\\{name}");
    // 动手前先记住目录本来是什么样：取消时要能干净地退回去。
    // 只有「本来就能启动的完整版本」才值得保留；半截目录、空目录都算这次安装的产物。
    // 判定和版本页保持一致：加载器版本自己没有 jar，真正的客户端 jar 在基版目录里。
    let base_id = v
        .get("inheritsFrom")
        .and_then(Value::as_str)
        .unwrap_or(&name)
        .to_string();
    let was_usable = version_usable(std::path::Path::new(&dir), &name, &base_id);
    std::fs::create_dir_all(&dir).map_err(|e| {
        format!("创建目录失败：{dir}（{e}）。若安装在受保护目录，请更换安装位置后重试")
    })?;
    // 版本隔离：每个版本一套自己的 mods / saves / config，互不相通。
    for sub in ["mods", "saves", "config", "logs", "resourcepacks", "shaderpacks", "crash-reports"] {
        let _ = std::fs::create_dir_all(format!("{dir}\\{sub}"));
    }
    let json_path = format!("{dir}\\{name}.json");
    std::fs::write(
        &json_path,
        serde_json::to_vec_pretty(&v).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("无法写入 {json_path}：{e}"))?;

    unsafe {
        h.emit_str(
            "mc.install.start",
            format!(
                r#"{{"id":"{id}","name":"{name}","files":{}}}"#,
                libs.len() + extra.len()
            )
            .as_bytes(),
        )
    };

    let all: Vec<Item> = libs.into_iter().chain(extra).collect();
    // 原版 jar 就躺在旁边的版本目录里时别再下一遍：38 MB，本地拷贝几十毫秒。
    // 拷过来的文件大小对得上，`dl::run` 会当成"已下好"直接跳过。
    if merged {
        for it in &all {
            if !it.path.starts_with("versions/") || !it.path.ends_with(".jar") {
                continue;
            }
            let dst = format!("{root}/{}", it.path);
            if std::path::Path::new(&dst).is_file() {
                continue;
            }
            let src = format!("{root}\\versions\\{id}\\{id}.jar");
            if let Ok(m) = std::fs::metadata(&src) {
                if it.size == 0 || m.len() == it.size {
                    if let Some(d) = std::path::Path::new(&dst).parent() {
                        let _ = std::fs::create_dir_all(d);
                    }
                    let _ = std::fs::copy(&src, &dst);
                }
            }
        }
    }
    let total_bytes: u64 = all.iter().map(|i| i.size).sum();
    let (task, cancel) = task_new("install", &format!("安装 {name}"), all.len() as u64);
    let got = fetch_all(h, &root, &all, concurrency(h), (all.len() / 50).max(1), Some(task), Some(&cancel));
    let ok = got.errors.is_empty() && !got.cancelled;

    // 取消 = 撤销这次安装。用户点"取消"想要的不是"留一堆下了半截的文件"，
    // 所以把本次新写的文件删掉；原本不存在的版本目录整个抹掉，
    // 原来是"补装"的就保留目录，只回滚新文件。
    let mut removed = 0usize;
    if got.cancelled {
        removed = task_rollback(task);
        // 取消 = 这次安装没发生过：本来不能用的目录整个抹掉（重试几次，
        // Windows 上刚写完的文件偶尔还被句柄占着）。
        if !was_usable {
            for _ in 0..5 {
                if std::fs::remove_dir_all(&dir).is_ok() || !std::path::Path::new(&dir).exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
            removed += 1;
        }
    }
    // 半成品必须留痕：没有这个标记，装失败的版本会跟正常版本一样躺在版本页里，
    // 用户点"启动"才发现 jar 根本没下来。取消不算失败，不留标记。
    let marker = format!("{dir}\\.incomplete");
    if got.cancelled {
        // 取消后要么目录已经没了，要么是用户原本就能用的版本：都不该带标记。
        let _ = std::fs::remove_file(&marker);
    } else if ok {
        let _ = std::fs::remove_file(&marker);
    } else {
        let _ = std::fs::write(&marker, got.errors.join("\n"));
    }
    if got.cancelled {
        task_cancelled(task, &format!("已取消，本次下载的 {removed} 个文件已清理"));
    } else {
        task_finish(task, ok, got.errors.first().map(String::as_str).unwrap_or(""));
    }

    let message = if got.cancelled {
        "已取消安装".to_string()
    } else {
        got.errors.first().cloned().unwrap_or_default()
    };
    // Fabric 版本装完顺手带上 Fabric API：绝大多数 Fabric 模组都依赖它。
    // 失败不影响安装结果，只把原因带回去让界面提示一句。
    // 整合包自己列了 Fabric API 时（`noFabricApi`）就别再塞一份，否则 mods 里会有两个。
    let mut fabric_api = String::new();
    let mut fabric_api_error = String::new();
    let skip_api = req.get("noFabricApi").and_then(Value::as_bool).unwrap_or(false);
    if ok && !skip_api && is_fabric_version(&v) {
        let mc_ver = v
            .get("inheritsFrom")
            .and_then(Value::as_str)
            .or_else(|| v.get("clientVersion").and_then(Value::as_str))
            .unwrap_or(id);
        match install_fabric_api(h, &dir, mc_ver) {
            Ok(f) => fabric_api = f,
            Err(e) => fabric_api_error = e,
        }
    }
    let payload = json!({
        "ok": ok,
        "error": message,
        "id": id,
        "name": name,
        "dir": dir,
        "root": root,
        "files": all.len(),
        "bytes": total_bytes,
        "downloaded": got.done,
        "cancelled": got.cancelled,
        "removed": removed,
        "ms": started.elapsed().as_millis() as u64,
        "fabricApi": fabric_api,
        "fabricApiError": fabric_api_error,
        "errors": got.errors,
    });
    unsafe {
        h.emit_str(
            "mc.install.done",
            format!(r#"{{"id":"{id}","name":"{name}","ok":{ok}}}"#).as_bytes(),
        )
    };
    Ok(serde_json::to_vec(&payload).unwrap_or_default())
}

// ---- 资源文件与启动 -------------------------------------------------------

/// 资源索引里的对象：`assets/objects/<前两位>/<sha1>`。
fn asset_items(index: &Value) -> Vec<Item> {
    let mirror = mirror_base();
    let mut out = Vec::new();
    let Some(map) = index.get("objects").and_then(Value::as_object) else {
        return out;
    };
    for v in map.values() {
        let (Some(hash), Some(size)) = (
            v.get("hash").and_then(Value::as_str),
            v.get("size").and_then(Value::as_u64),
        ) else {
            continue;
        };
        if hash.len() < 3 {
            continue;
        }
        let two = &hash[..2];
        out.push(Item {
            path: format!("assets/objects/{two}/{hash}"),
            url: format!("{mirror}/assets/{two}/{hash}"),
            size,
            // 资源对象的文件名就是 sha1，校验是免费的。
            sha1: Some(hash.to_string()),
        });
    }
    out
}

fn service_assets(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let id = field(req, "id").ok_or("缺 id")?;
    let root = game_root(h, req);
    // 资源索引的名字不是版本号（26.3 用的是 "34"），得从解析后的版本里取。
    let v = resolve(h, &manifest_for(h, id), id, 0)?;
    let idx = v.get("assetIndex").ok_or("该版本缺少资源索引信息")?;
    let index_id = idx.get("id").and_then(Value::as_str).unwrap_or("legacy");

    let index_path = format!("{root}\\assets\\indexes\\{index_id}.json");
    if !std::path::Path::new(&index_path).exists() {
        if let Some(url) = idx.get("url").and_then(Value::as_str) {
            download(h, url, &index_path, 0)?;
        }
    }
    let text = std::fs::read_to_string(&index_path)
        .map_err(|e| format!("无法读取资源索引 {index_path}：{e}"))?;
    let index: Value = serde_json::from_str(&text).map_err(|e| format!("索引不是 JSON：{e}"))?;
    let items = asset_items(&index);
    // 运行库也一起补：加载器版本（Fabric/Quilt）的库不在原版清单里，
    // 只补资源的话启动时还是会报「缺少 N 个运行库文件」。
    let mut items = items;
    let (libs, _) = plan(&v, id);
    for it in libs {
        let p = format!("{root}\\{}", it.path.replace('/', "\\"));
        if !std::path::Path::new(&p).is_file() {
            items.push(it);
        }
    }
    let bytes: u64 = items.iter().map(|i| i.size).sum();
    let total = items.len();
    unsafe {
        h.emit_str(
            "mc.assets.start",
            format!(r#"{{"files":{total},"bytes":{bytes}}}"#).as_bytes(),
        )
    };
    let started = std::time::Instant::now();
    let (task, cancel) = task_new("assets", "补齐资源文件", total as u64);
    let got = fetch_all(h, &root, &items, concurrency(h), (items.len() / 100).max(1), Some(task), Some(&cancel));
    let ok = got.errors.is_empty() && !got.cancelled;
    // 资源是共享缓存，但"取消"就意味着这次别留下痕迹：本次新下的对象按清单删掉。
    let removed = if got.cancelled { task_rollback(task) } else { 0 };
    if got.cancelled {
        task_cancelled(task, &format!("已取消，本次下载的 {removed} 个文件已清理"));
    } else {
        task_finish(task, ok, got.errors.first().map(String::as_str).unwrap_or(""));
    }
    let ms = started.elapsed().as_millis() as u64;
    // 下完再数一遍还缺几个：网络抖动时"失败 0"也可能有漏网的，
    // 这个数字直接决定用户能不能启动，得如实给出来。
    let still_missing = items
        .iter()
        .filter(|i| {
            !std::path::Path::new(&format!("{root}\\{}", i.path.replace('/', "\\"))).exists()
        })
        .count();
    unsafe {
        h.emit_str(
            "mc.assets.done",
            format!(
                r#"{{"files":{total},"errors":{},"missing":{still_missing},"ms":{ms}}}"#,
                got.errors.len()
            )
            .as_bytes(),
        )
    };
    Ok(serde_json::to_vec(&json!({
        "ok": ok && still_missing == 0,
        "cancelled": got.cancelled,
        "removed": removed,
        "files": total,
        "downloaded": got.done,
        "bytes": bytes,
        "errors": got.errors.len(),
        "missing": still_missing,
        "ms": ms,
        "first": got.errors.first(),
    }))
    .unwrap_or_default())
}

/// 一个条目在磁盘上是否完好：大小对得上、有 sha1 就再比一次哈希。
///
/// 只认「大小 + 哈希」两个条件：镜像返回 200 也可能是错误页，
/// 光看文件存在会让坏文件一直躺在那里。
fn file_ok(root: &str, it: &Item) -> bool {
    let p = std::path::Path::new(root).join(it.path.replace('/', "\\"));
    let Ok(meta) = std::fs::metadata(&p) else {
        return false;
    };
    if it.size > 0 && meta.len() != it.size {
        return false;
    }
    match &it.sha1 {
        Some(want) => sha1_of_file(&p)
            .map(|h| h.eq_ignore_ascii_case(want))
            .unwrap_or(false),
        None => true,
    }
}

/// 数一数还缺多少运行库 / 资源对象。
///
/// 启动前自检和「补齐后复查」用的是同一套判据，抽出来免得两处漂移：
/// 运行库看文件在不在（类路径最后一项是客户端 jar，单独算），资源对象
/// 还要看长度对不对 —— 中断过的下载会留下长度不对的文件，游戏读到它
/// 一样在 TextureManager / LinkFSProvider 里炸，而且崩得比缺文件更晚。
fn missing_counts(
    root: &str,
    cp: &[String],
    native_jars: &[String],
    index: Option<&Value>,
) -> (usize, usize) {
    let libs = cp[..cp.len().saturating_sub(1)]
        .iter()
        .filter(|p| !std::path::Path::new(p.as_str()).is_file())
        .count()
        + native_jars
            .iter()
            .filter(|p| !std::path::Path::new(p.as_str()).is_file())
            .count();
    let assets = match index {
        Some(ix) => asset_items(ix)
            .iter()
            .filter(|i| {
                let path = format!("{root}\\{}", i.path.replace('/', "\\"));
                match std::fs::metadata(&path) {
                    Ok(meta) => i.size > 0 && meta.len() != i.size,
                    Err(_) => true,
                }
            })
            .count(),
        None => 0,
    };
    (libs, assets)
}

/// 资源重补：把已装版本里缺的、大小不对的、sha1 不对的文件重新下回来。
///
/// 只补不删：坏文件会被覆盖，多余文件一律不动（用户自己塞的 mod 不能被清掉）。
fn service_repair(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let name = field(req, "name")
        .or_else(|| field(req, "id"))
        .ok_or("缺 name")?;
    let root = game_root(h, req);
    let dir = format!("{root}\\versions\\{name}");
    if !std::path::Path::new(&dir).is_dir() {
        return Err(format!("没有这个版本：{name}"));
    }
    let v = resolve_local(&root, name).ok_or_else(|| format!("读不到 {name}.json，先重新安装"))?;
    let (libs, extra) = plan(&v, name);
    let mut all: Vec<Item> = libs.into_iter().chain(extra).collect();
    // 资源对象：索引在本地就用本地的；索引本身缺了，交给 assets 流程去补。
    if let Some(idx) = v.get("assetIndex") {
        let index_id = idx.get("id").and_then(Value::as_str).unwrap_or("legacy");
        let index_path = format!("{root}\\assets\\indexes\\{index_id}.json");
        if let Ok(text) = std::fs::read_to_string(&index_path) {
            if let Ok(index) = serde_json::from_str::<Value>(&text) {
                all.extend(asset_items(&index));
            }
        }
    }
    let checked = all.len();
    let bad: Vec<Item> = all.into_iter().filter(|it| !file_ok(&root, it)).collect();
    let missing = bad.len();
    if missing == 0 {
        return Ok(serde_json::to_vec(&json!({
            "ok": true, "name": name, "checked": checked,
            "missing": 0, "fixed": 0, "errors": 0
        }))
        .unwrap_or_default());
    }
    let started = std::time::Instant::now();
    let (task, cancel) = task_new("repair", &format!("修复 {name}"), missing as u64);
    let got = fetch_all(
        h,
        &root,
        &bad,
        concurrency(h),
        (missing / 50).max(1),
        Some(task),
        Some(&cancel),
    );
    let ok = got.errors.is_empty() && !got.cancelled;
    task_finish(task, ok, if ok { "" } else { "有文件没能补回来" });
    let still = bad.iter().filter(|it| !file_ok(&root, it)).count();
    let ms = started.elapsed().as_millis() as u64;
    unsafe {
        h.emit_str(
            "mc.repair.done",
            format!(
                r#"{{"name":"{name}","missing":{missing},"fixed":{},"still":{still},"ms":{ms}}}"#,
                missing - still
            )
            .as_bytes(),
        )
    };
    Ok(serde_json::to_vec(&json!({
        "ok": ok && still == 0,
        "name": name,
        "checked": checked,
        "missing": missing,
        "fixed": missing - still,
        "still": still,
        "errors": got.errors.len(),
        "first": got.errors.first(),
        "ms": ms,
    }))
    .unwrap_or_default())
}

fn subst(text: &str, table: &[(&str, String)]) -> String {
    let mut out = text.to_string();
    for (k, v) in table {
        out = out.replace(&format!("${{{k}}}"), v);
    }
    out
}

/// 展开 `arguments.game`：数组里的每项要么是字符串，要么是带 rules 的对象。
fn game_args(version: &Value, table: &[(&str, String)]) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(list) = version
        .get("arguments")
        .and_then(|a| a.get("game"))
        .and_then(Value::as_array)
    {
        for entry in list {
            match entry {
                Value::String(s) => out.push(subst(s, table)),
                Value::Object(_) => {
                    if allows(entry.get("rules")) {
                        match entry.get("value") {
                            Some(Value::String(s)) => out.push(subst(s, table)),
                            Some(Value::Array(a)) => {
                                for v in a {
                                    if let Some(s) = v.as_str() {
                                        out.push(subst(s, table));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    } else if let Some(legacy) = version.get("minecraftArguments").and_then(Value::as_str) {
        // 老版本是一整根字符串
        for part in legacy.split_whitespace() {
            out.push(subst(part, table));
        }
    }
    out
}

// ---- 对齐 PCL2 的启动前处理 ------------------------------------------------
//
// 三件事是本文件原先没有、而 PCL2 一定做的：
// 1. 按版本代际把 options.txt 的 `lang` 设成中文（1.10- 要 `zh_CN`，1.11+ 要 `zh_cn`，
//    写错轻则自动切回英文，重则找不到语言文件直接崩）；
// 2. 全屏 / 窗口模式写进 options.txt，全屏再补 `--fullscreen`；
// 3. JVM 参数用版本 JSON 里的 `arguments.jvm` 展开，而不是自己硬拼。

/// 设置快照。启动这条路上要读语言、窗口、分辨率，读一次就够。
fn settings_snapshot(h: &MiyHostV1) -> Value {
    call_json(h, "miy.app.settings", &json!({}))
        .ok()
        .and_then(|v| v.get("values").cloned())
        .unwrap_or(Value::Null)
}

fn setting_bool(snap: &Value, key: &str, default: bool) -> bool {
    snap.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn setting_u64(snap: &Value, key: &str, default: u64) -> u64 {
    snap.get(key).and_then(Value::as_u64).unwrap_or(default)
}

fn setting_str(snap: &Value, key: &str) -> String {
    snap.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 窗口模式，编号和 PCL2 的 `LaunchArgumentWindowType` 一一对应：
/// 0 全屏 / 1 默认 / 2 与启动器同尺寸 / 3 自定义（用设置里的宽高）。
/// 存的是给用户看的枚举文字，所以按文字认；认不出来就是「跟随上次」——什么都不改最安全。
fn setting_window(snap: &Value) -> u64 {
    match snap.get("miy.mc.window").and_then(Value::as_str) {
        Some("全屏") => 0,
        Some("与启动器同尺寸") => 2,
        Some("窗口化") => 3,
        _ => 1,
    }
}

/// GC 设置，对应 PCL2 的 `LaunchAdvanceGC`：0 自动 / 4 优化 G1GC / 2 仅 G1GC / 3 不指定。
fn setting_gc(snap: &Value) -> u64 {
    match snap.get("miy.mc.gc").and_then(Value::as_str) {
        Some("G1GC（优化）") => 4,
        Some("仅 G1GC") => 2,
        Some("不指定") => 3,
        _ => 0,
    }
}

/// 这个版本真正对应的原版版本号（加载器版本顺着 `inheritsFrom` 往上走）。
/// 语言的**大小写**规则是按原版大版本分的，看加载器自己的版本号会判错。
fn vanilla_id(root: &str, version: &Value, id: &str) -> String {
    let mut cur = id.to_string();
    let mut node = version.clone();
    for _ in 0..6 {
        let Some(parent) = node
            .get("inheritsFrom")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            break;
        };
        let path = format!("{root}\\versions\\{parent}\\{parent}.json");
        let next = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        cur = parent;
        match next {
            Some(v) => node = v,
            None => break,
        }
    }
    cur
}

/// 原版大版本号：`1.12.2` → 12。26.x 这种新版本号一律当「现代版本」（99）。
fn version_major(id: &str) -> u32 {
    let mut parts = id.split('.');
    match parts.next() {
        Some("1") => parts
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(99),
        _ => 99,
    }
}

/// 读文本并剥掉 UTF-8 BOM。别的启动器、记事本、Git for Windows 写的文件
/// 常带 BOM；不剥的话首行的键名会变成 `\u{feff}lang`，永远匹配不上，
/// 于是每启动一次就多写一行 `lang:...`（游戏读最后一条，行为看着正常，
/// 文件却在悄悄长胖）。
fn read_text(path: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.trim_start_matches('\u{feff}').to_string())
}

/// 读 options.txt 的一个 `key:value`。文件不存在返回 None。
fn read_ini(path: &str, key: &str) -> Option<String> {
    let text = read_text(path)?;
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, val)) = line.split_once(':') {
            if k.trim() == key {
                found = Some(val.trim().to_string());
            }
        }
    }
    found
}

/// 写 options.txt 的一个键，**保留其他所有行**（包括认不出的键）。
/// MC 只认 `key:value`，多认一个键就多一份兼容性，没必要替玩家清理文件。
fn write_ini(path: &str, key: &str, value: &str) -> bool {
    let old = read_text(path).unwrap_or_default();
    let crlf = old.contains("\r\n");
    let mut lines: Vec<String> = old
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect();
    let mut hit = false;
    for line in lines.iter_mut() {
        if let Some((k, _)) = line.split_once(':') {
            if k.trim() == key {
                *line = format!("{key}:{value}");
                hit = true;
            }
        }
    }
    if !hit {
        lines.push(format!("{key}:{value}"));
    }
    let mut text = lines.join(if crlf { "\r\n" } else { "\n" });
    text.push_str(if crlf { "\r\n" } else { "\n" });
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, text).is_ok()
}

/// 语言代码换成玩家看得懂的名字（界面上不出现 `zh_cn` 这种东西）。
fn lang_label(code: &str) -> &'static str {
    match code.to_lowercase().as_str() {
        "zh_cn" => "简体中文",
        "zh_tw" => "繁体中文",
        "en_us" => "英语（美国）",
        _ => "游戏默认语言",
    }
}

/// Java 的来源同样换成说明性文字。
fn java_source_label(source: &str) -> &'static str {
    match source {
        "启动器下载" => "启动器自带",
        "JAVA_HOME" | "PATH" => "系统环境变量",
        _ => "系统中已安装",
    }
}

/// 返回给界面看的说明（启动器动过什么，得让用户知道）。
fn sync_options_txt(root: &str, id: &str, version: &Value, snap: &Value) -> Vec<String> {
    let mut notes: Vec<String> = Vec::new();
    let indie = format!("{root}\\versions\\{id}");
    let mut options = format!("{indie}\\options.txt");

    // Yosbr 这类 mod 会把默认选项挪到 config/yosbr/options.txt，
    // 只改外面的 options.txt 游戏起来还是英文（PCL2 #2385）。
    if !std::path::Path::new(&options).is_file() {
        let yosbr = format!("{indie}\\config\\yosbr\\options.txt");
        if std::path::Path::new(&yosbr).is_file() {
            let _ = write_ini(&yosbr, "lang", "none");
            notes.push("检测到 Yosbr，语言设置将写入其配置文件。".to_string());
            options = yosbr;
        }
    }

    // 和 PCL2 一致：只有「玩家还没选过语言」才替他设中文。
    // 判据是 lang 缺失，或者还没有 saves 目录（一次都没进过世界）。
    let current = read_ini(&options, "lang").unwrap_or_else(|| "none".to_string());
    let first_run =
        current == "none" || !std::path::Path::new(&format!("{indie}\\saves")).is_dir();
    let base: String = if first_run {
        if setting_bool(snap, "miy.mc.chinese", true) {
            "zh_cn".to_string()
        } else {
            "en_us".to_string()
        }
    } else {
        current.to_lowercase()
    };

    // 1.10 及以前只认大写的国家码：`zh_cn` 找不到语言文件，切过去直接 NPE 崩游戏。
    // 1.11 以后反过来：`zh_CN` 会被当成未知语言，显示中文但语言设置回落成英文。
    let vanilla = vanilla_id(root, version, id);
    let mut required = base.to_string();
    if version_major(&vanilla) <= 10 && required.len() >= 2 {
        let cut = required.len() - 2;
        required = format!("{}{}", &required[..cut], required[cut..].to_uppercase());
    }

    if current == required {
        notes.push(format!("游戏语言已是{}，无需修改。", lang_label(&required)));
    } else {
        // 先写个占位值再写真值：MC 和 PCL 都有按文件缓存解析结果的路径，
        // 只写一次有时读不到（PCL2 的原注释：触发缓存更改）。
        let _ = write_ini(&options, "lang", "-");
        if write_ini(&options, "lang", &required) {
            notes.push(format!("已将游戏语言设置为{}。", lang_label(&required)));
        }
    }

    // 窗口模式：老版本没有命令行开关，写 options.txt 是唯一通用的办法。
    let window = setting_window(snap);
    if window != 1 {
        let value = if window == 0 { "true" } else { "false" };
        if read_ini(&options, "fullscreen").as_deref() != Some(value) {
            let _ = write_ini(&options, "fullscreen", value);
            notes.push(format!(
                "窗口模式已设为{}启动。",
                if window == 0 { "全屏" } else { "窗口化" }
            ));
        }
    }
    notes
}

/// 展开版本 JSON 的 `arguments.jvm`。返回 None 表示这是 ≤1.12 的老版本，
/// 调用方改用手写参数。PCL2 新版走的也是这条路 —— 硬编码必然漏掉
/// `--enable-native-access`、`-Dorg.lwjgl.system.SharedLibraryExtractPath` 这类东西。
fn jvm_args_json(version: &Value, table: &[(&str, String)]) -> Option<Vec<String>> {
    let list = version.get("arguments")?.get("jvm")?.as_array()?;
    let mut raw: Vec<String> = Vec::new();
    for entry in list {
        match entry {
            Value::String(s) => raw.push(subst(s, table)),
            Value::Object(_) => {
                if allows(entry.get("rules")) {
                    match entry.get("value") {
                        Some(Value::String(s)) => raw.push(subst(s, table)),
                        Some(Value::Array(a)) => {
                            for v in a {
                                if let Some(s) = v.as_str() {
                                    raw.push(subst(s, table));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    // `-cp ${classpath}` 由调用方统一追加：出现两个 -cp 时，第二个会把
    // mainClass 当成类路径的一部分，报 "Could not find or load main class"。
    let mut cleaned: Vec<String> = Vec::new();
    let mut skip = false;
    for arg in raw {
        if skip {
            skip = false;
            continue;
        }
        if arg == "-cp" || arg == "-classpath" {
            skip = true;
            continue;
        }
        cleaned.push(arg);
    }
    // 按完整参数去重、保留最后一次出现（JSON 里确实有重复参数）。
    Some(dedupe_keep_last(cleaned))
}

/// 把一条管道逐行转成 `mc.log`。按字节读到换行再 lossy 解码 ——
/// `lines()` 碰到一行不是 UTF-8 就直接结束，GBK 的 stderr 会让日志半路断掉。
fn forward_lines<R: std::io::Read>(pipe: R, cell: &HostCell, prefix: &str) {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(pipe);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf)
            .trim_end_matches(['\r', '\n'])
            .to_string();
        if line.is_empty() {
            continue;
        }
        let payload = serde_json::to_vec(&json!({ "line": format!("{prefix}{line}") })).unwrap_or_default();
        unsafe { cell.h().emit_str("mc.log", &payload) };
    }
}

// ---- 对齐 PCL2 的启动前处理（续）：natives / 虚拟资源 / GC / 自定义参数 ----

struct CrashInfo {
    reason: String,
    /// 怎么办。规则没命中时为空，界面就不显示这一行。
    advice: String,
    /// 命中的规则名，写进日志方便排查。
    rule: &'static str,
    log: String,
    crash: String,
}

/// 版本目录现在能不能直接启动 —— 和版本页的判定保持一致。
/// 加载器版本（Fabric/Forge）自己没有 jar，真正的客户端 jar 在基版目录里，
/// 所以不能只看 `{name}.jar`，否则重装加载器版本时取消会把整个目录（含存档、模组）删掉。
fn version_usable(dir: &std::path::Path, name: &str, base: &str) -> bool {
    if !dir.is_dir() {
        return false;
    }
    if dir.join(".incomplete").is_file() {
        return false;
    }
    if !dir.join(format!("{name}.json")).is_file() {
        return false;
    }
    let jar_ok = std::fs::metadata(dir.join(format!("{name}.jar")))
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    let base_jar = dir
        .parent()
        .map(|p| p.join(base).join(format!("{base}.jar")))
        .map(|p| p.is_file())
        .unwrap_or(false);
    jar_ok || base_jar
}

/// 从版本目录里找崩溃原因：先跑规则表，没命中再退回 crash-report 的 Description。
/// PCL2 的崩溃窗口也是这么做的 —— 玩家要的是「为什么崩、怎么办」，不是「崩了」。
fn crash_info(dir: &str, code: i32, java_major: Option<u32>, required: Option<u32>) -> CrashInfo {
    let mut crash_path = String::new();
    let mut crash_text = String::new();
    let reports = std::path::Path::new(dir).join("crash-reports");
    let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    if let Ok(rd) = std::fs::read_dir(&reports) {
        for e in rd.flatten() {
            let p = e.path();
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !name.starts_with("crash-") || !name.ends_with(".txt") {
                continue;
            }
            let t = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            if newest.as_ref().map(|(nt, _)| t > *nt).unwrap_or(true) {
                newest = Some((t, p));
            }
        }
    }
    if let Some((_, p)) = newest {
        crash_path = p.to_string_lossy().into_owned();
        if let Some(text) = read_text(&crash_path) {
            crash_text = text;
        }
    }
    let mut log = String::new();
    if let Some(t) = read_text(&format!("{dir}\\logs\\latest.log")) {
        let lines: Vec<&str> = t.lines().collect();
        let start = lines.len().saturating_sub(80);
        log = lines[start..].join("\n");
    }

    // 规则表：把堆栈翻成「人话 + 怎么办」。命中就不再看 Description 了 ——
    // Description 是给开发者看的（"Rendering overlay"），玩家看不懂。
    let facts = crash::Facts {
        crash: &crash_text,
        log: &log,
        code,
        java_major,
        required_java: required,
    };
    if let Some(d) = crash::diagnose(&facts) {
        return CrashInfo {
            reason: d.title,
            advice: d.advice,
            rule: d.rule,
            log,
            crash: crash_path,
        };
    }

    let mut reason = format!("游戏异常退出（退出代码 {code}）");
    let mut lines = crash_text.lines();
    while let Some(l) = lines.next() {
        if let Some(rest) = l.trim_start().strip_prefix("Description:") {
            let head = rest.trim();
            let next = lines.next().unwrap_or("").trim();
            reason = if next.is_empty() {
                head.to_string()
            } else {
                format!("{head}：{next}")
            };
            break;
        }
    }
    CrashInfo { reason, advice: String::new(), rule: "", log, crash: crash_path }
}


/// PCL2 的 `SplitJavaArguments`：空格分隔，双引号里的空格不切。
/// 引号本身丢掉 —— 我们的参数是逐条交给 `Command::args` 的，转义由系统负责。
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ' ' | '\t' | '\r' | '\n' if !in_quotes => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 自定义参数里的 `${...}` 展开（PCL2 的 ArgumentReplace）。
/// 先切分成参数再替换，路径里带空格也不会被拆开。
fn arg_replace(s: &str, table: &[(&str, String)]) -> String {
    let mut out = s.to_string();
    for (key, value) in table {
        out = out.replace(&format!("${{{key}}}"), value);
    }
    out
}

/// 去重但保留最后一次出现，等价于 PCL2 的 `DeduplicateJavaArguments(…, True)`。
/// 自定义 `-Xmx` 能盖掉启动器自己的内存参数，靠的就是这条规则。
fn dedupe_keep_last(args: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    for arg in args.into_iter().rev() {
        if !out.contains(&arg) {
            out.push(arg);
        }
    }
    out.reverse();
    out
}

struct ZipEntry {
    name: String,
    method: u16,
    comp_off: usize,
    comp_len: usize,
    size: usize,
}

/// 极简 ZIP 解析：只读中央目录，够解压 natives 包（stored / deflate 两种方法）。
/// 自己写而不拉一个 zip 依赖：这里只需要「列举 + 读一个 entry」，
/// 依赖的 API 面和编译体积都比这一百多行贵。
fn zip_list(bytes: &[u8]) -> Result<Vec<ZipEntry>, String> {
    let u16_at = |p: usize| bytes.get(p..p + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |p: usize| bytes.get(p..p + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));

    if bytes.len() < 22 {
        return Err("文件太小，不像 ZIP".to_string());
    }
    // 结尾记录离文件尾最多 64 KB 注释，从后往前找。
    let stop = bytes.len().saturating_sub(22 + 0xFFFF);
    let mut p = bytes.len() - 22;
    let mut eocd = None;
    loop {
        if bytes.get(p..p + 4) == Some(&[0x50, 0x4b, 0x05, 0x06]) {
            eocd = Some(p);
            break;
        }
        if p == stop {
            break;
        }
        p -= 1;
    }
    let eocd = eocd.ok_or("找不到 ZIP 中央目录（文件可能已损坏）")?;

    let count = u16_at(eocd + 10).unwrap_or(0) as usize;
    let mut cur = u32_at(eocd + 16).unwrap_or(0) as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if bytes.get(cur..cur + 4) != Some(&[0x50, 0x4b, 0x01, 0x02]) {
            return Err("ZIP 中央目录损坏".to_string());
        }
        let method = u16_at(cur + 10).ok_or("ZIP 中央目录损坏")?;
        let comp_len = u32_at(cur + 20).ok_or("ZIP 中央目录损坏")? as usize;
        let size = u32_at(cur + 24).ok_or("ZIP 中央目录损坏")? as usize;
        let name_len = u16_at(cur + 28).ok_or("ZIP 中央目录损坏")? as usize;
        let extra_len = u16_at(cur + 30).ok_or("ZIP 中央目录损坏")? as usize;
        let comment_len = u16_at(cur + 32).ok_or("ZIP 中央目录损坏")? as usize;
        let local = u32_at(cur + 42).ok_or("ZIP 中央目录损坏")? as usize;
        let name_bytes = bytes
            .get(cur + 46..cur + 46 + name_len)
            .ok_or("ZIP 中央目录损坏")?;
        let name = String::from_utf8_lossy(name_bytes).to_string();

        // 本地文件头是 30 字节定长 + 文件名 + extra，数据紧跟其后。
        // 长度必须以中央目录为准：流式写出的包会在本地头里填 0。
        if bytes.get(local..local + 4) != Some(&[0x50, 0x4b, 0x03, 0x04]) {
            return Err("ZIP 本地文件头损坏".to_string());
        }
        let l_name = u16_at(local + 26).ok_or("ZIP 本地文件头损坏")? as usize;
        let l_extra = u16_at(local + 28).ok_or("ZIP 本地文件头损坏")? as usize;
        let comp_off = local + 30 + l_name + l_extra;

        out.push(ZipEntry {
            name,
            method,
            comp_off,
            comp_len,
            size,
        });
        cur += 46 + name_len + extra_len + comment_len;
    }
    Ok(out)
}

fn zip_read(bytes: &[u8], e: &ZipEntry) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let data = bytes
        .get(e.comp_off..e.comp_off + e.comp_len)
        .ok_or("ZIP 数据越界")?;
    let mut out: Vec<u8> = Vec::with_capacity(e.size);
    match e.method {
        0 => out.extend_from_slice(data),
        8 => {
            flate2::read::DeflateDecoder::new(data)
                .read_to_end(&mut out)
                .map_err(|err| format!("解压 {} 失败：{err}", e.name))?;
        }
        method => return Err(format!("不支持的压缩方法 {method}（{}）", e.name)),
    }
    Ok(out)
}

/// 包内路径只允许普通相对路径：堵住 `..\..\Windows\...` 这种写到 natives 目录外面的包。
fn safe_rel(name: &str) -> Option<String> {
    let normalized = name.replace('\\', "/");
    let mut parts: Vec<String> = Vec::new();
    for part in normalized.split('/') {
        match part {
            "" | "." => continue,
            ".." => return None,
            p if p.contains(':') => return None,
            p => parts.push(p.to_string()),
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\\"))
    }
}

/// 版本里所有「natives 包」（PCL2 的 `IsNatives`）：带 `natives.windows` 的运行库。
/// 1.19+ 把 natives 拆成了普通库条目，不再走这里 —— 那些由 LWJGL 自己在运行时解压。
fn natives_jars(root: &str, version: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(list) = version.get("libraries").and_then(Value::as_array) else {
        return out;
    };
    for lib in list {
        if !allows(lib.get("rules")) {
            continue;
        }
        let Some(native) = lib
            .get("natives")
            .and_then(|n| n.get("windows"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let native = native.replace(
            "${arch}",
            if cfg!(target_pointer_width = "64") {
                "64"
            } else {
                "32"
            },
        );
        // 有 classifiers 就用它的 path（下载时就是按这个落盘的）。
        if let Some(path) = lib
            .get("downloads")
            .and_then(|d| d.get("classifiers"))
            .and_then(|c| c.get(native.as_str()))
            .and_then(|x| x.get("path"))
            .and_then(Value::as_str)
        {
            out.push(format!("{root}\\libraries\\{}", path.replace('/', "\\")));
            continue;
        }
        // 老清单可能只有 name：按 Maven 规则把它推出来。
        if let Some(path) = lib
            .get("downloads")
            .and_then(|d| d.get("artifact"))
            .and_then(|a| a.get("path"))
            .and_then(Value::as_str)
        {
            out.push(format!(
                "{root}\\libraries\\{}",
                path.replace('/', "\\").replace(".jar", &format!("-{native}.jar"))
            ));
        }
    }
    out
}

/// 解压 natives 包里的 dll，照 PCL2 `McLaunchNatives` 的做法：
/// 只认 `.dll`；目标文件长度相同就跳过；最后删掉这次没解出来的旧 dll。
fn extract_natives(jars: &[String], dest: &str, notes: &mut Vec<String>) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("建 natives 目录失败：{e}"))?;
    let mut keep: Vec<String> = Vec::new();
    let mut extracted = 0usize;
    for jar in jars {
        let bytes = std::fs::read(jar).map_err(|e| format!("读 natives 包失败：{jar}（{e}）"))?;
        let entries =
            zip_list(&bytes).map_err(|e| format!("无法打开 natives 包 {jar}：{e}，请重新下载该文件"))?;
        for entry in &entries {
            if !entry.name.to_lowercase().ends_with(".dll") {
                continue;
            }
            // 大小写不敏感：Windows 上 Lib1.DLL 和 lib1.dll 是同一个文件。
            let rel = safe_rel(&entry.name)
                .ok_or_else(|| format!("natives 包里有可疑路径：{}", entry.name))?;
            let target = format!("{dest}\\{rel}");
            keep.push(target.to_lowercase());
            if let Ok(meta) = std::fs::metadata(&target) {
                if meta.len() as usize == entry.size {
                    continue;
                }
                let _ = std::fs::remove_file(&target);
            }
            if let Some(parent) = std::path::Path::new(&target).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let data = zip_read(&bytes, entry)?;
            std::fs::write(&target, &data).map_err(|e| format!("写 {target} 失败：{e}"))?;
            extracted += 1;
        }
    }

    // 旧版本换新版时 natives 目录会残留同名 dll，游戏会加载到旧的那个。
    // 只清顶层文件：java/jna/lwjgl/netty 这几个子目录是现代版本要用的。
    if let Ok(read) = std::fs::read_dir(dest) {
        for ent in read.flatten() {
            let path = ent.path();
            if !path.is_file() || !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("dll")) {
                continue;
            }
            if !keep.contains(&path.to_string_lossy().to_lowercase()) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    if extracted > 0 {
        notes.push(format!("已解压 {extracted} 个运行库文件。"));
    }
    Ok(())
}

/// 1.6 以前的资源索引是「虚拟」的：游戏按原始路径读 `assets/virtual/legacy/minecraft/...`，
/// 而不是 `objects/<hash>`。PCL2 下载时就直接写进虚拟目录，我们统一在启动前摆好。
fn sync_virtual_assets(root: &str, version_dir: &str, assets_id: &str) -> Result<usize, String> {
    let index_path = format!("{root}\\assets\\indexes\\{assets_id}.json");
    let Ok(text) = std::fs::read_to_string(&index_path) else {
        return Ok(0);
    };
    let Ok(index) = serde_json::from_str::<Value>(&text) else {
        return Ok(0);
    };
    let remap = index
        .get("map_to_resources")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let virtual_index = index.get("virtual").and_then(Value::as_bool).unwrap_or(false);
    if !remap && !virtual_index {
        return Ok(0);
    }
    let base = if remap {
        format!("{version_dir}\\resources")
    } else {
        format!("{root}\\assets\\virtual\\legacy")
    };
    let mut copied = 0usize;
    if let Some(objects) = index.get("objects").and_then(Value::as_object) {
        for (name, obj) in objects {
            let hash = obj.get("hash").and_then(Value::as_str).unwrap_or("");
            let size = obj.get("size").and_then(Value::as_u64).unwrap_or(0);
            if hash.len() < 2 {
                continue;
            }
            let src = format!("{root}\\assets\\objects\\{}\\{hash}", &hash[..2]);
            let dst = format!("{base}\\{}", name.replace('/', "\\"));
            if std::fs::metadata(&dst).is_ok_and(|m| size == 0 || m.len() == size) {
                continue;
            }
            if let Some(parent) = std::path::Path::new(&dst).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::copy(&src, &dst).map_err(|e| format!("复制资源 {name} 失败：{e}"))?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// 系统 build 号。ZGC 要 Windows 10 1809（build 17763）以上，低版本只能用 G1GC。
fn windows_build() -> u32 {
    let Ok(out) = std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "/v",
            "CurrentBuildNumber",
        ])
        .output()
    else {
        return 0;
    };
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .find_map(|t| t.parse::<u32>().ok())
        .unwrap_or(0)
}

/// PCL2 会先删掉已有的 GC 参数再加自己的，避免版本 JSON 里带的参数打架。
fn is_gc_arg(arg: &str) -> bool {
    let Some(rest) = arg.strip_prefix("-XX:") else {
        return false;
    };
    rest.contains("GC")
        || rest.contains("G1")
        || rest.contains("ZGenerational")
        || rest.contains("UseCompactObjectHeaders")
        || rest.contains("HeapFreeRatio")
}

/// GC 参数，完全照 PCL2 `McLaunchArgumentMain` 的「内存管理」段：
/// 0 自动（Java 15+ 用 ZGC，21/22 再补 ZGenerational；15- 用 G1GC）、
/// 4 优化的 G1GC、2 仅 G1GC、3 不指定。
fn apply_gc_args(args: &mut Vec<String>, java: u32, mode: u64, build: u32) {
    if mode == 3 {
        return;
    }
    args.retain(|a| !is_gc_arg(a));
    let mut use_g1 = (mode == 0 && java < 15) || mode == 2 || mode == 4;
    // 系统太老就没有 ZGC：PCL2 在 1809 以下强制回落 G1GC。
    if build != 0 && build < 17763 {
        use_g1 = true;
    }
    args.push("-XX:+UnlockExperimentalVMOptions".to_string());
    if java >= 24 {
        // JDK 24 起 PCL2 会开紧凑对象头，省内存。
        args.push("-XX:+UseCompactObjectHeaders".to_string());
    }
    if use_g1 {
        args.push("-XX:+UseG1GC".to_string());
        args.push("-XX:G1NewSizePercent=20".to_string());
        args.push("-XX:G1ReservePercent=20".to_string());
        args.push("-XX:G1HeapRegionSize=32M".to_string());
        args.push("-XX:MaxGCPauseMillis=50".to_string());
        if mode == 4 {
            args.push("-XX:+PerfDisableSharedMem".to_string());
            if java == 8 {
                args.push("-XX:+ParallelRefProcEnabled".to_string());
            }
            if java >= 12 {
                args.push("-XX:MinHeapFreeRatio=25".to_string());
                args.push("-XX:MaxHeapFreeRatio=40".to_string());
            }
        }
        if java <= 7 {
            args.push("-XX:MaxPermSize=512m".to_string());
        }
    } else {
        args.push("-XX:+UseZGC".to_string());
        if java == 21 || java == 22 {
            // Java 23 起默认就是分代 ZGC，不用再加。
            args.push("-XX:+ZGenerational".to_string());
        }
    }
}

/// 启动器窗口的客户区尺寸（物理像素），给「与启动器同尺寸」用。
fn launcher_size(h: &MiyHostV1) -> Option<(u64, u64)> {
    let info = call_json(h, "miy.app.info", &json!({})).ok()?;
    let window = info.get("window")?;
    let w = window.get("width").and_then(Value::as_u64)?;
    let hh = window.get("height").and_then(Value::as_u64)?;
    if w > 200 && hh > 150 {
        Some((w, hh))
    } else {
        None
    }
}

/// 去掉已有的 `--flag 值` 对：PCL2 用「后出现的覆盖先出现的」,
/// 我们直接把旧的删掉，免得游戏收到两个宽高。
fn drop_flag_pair(args: &mut Vec<String>, flag: &str) {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            args.remove(i);
            if i < args.len() {
                args.remove(i);
            }
        } else {
            i += 1;
        }
    }
}

/// `miy.mc.javas` —— 给界面看的 Java 清单：这个版本要哪个主版本、现在会选谁、机器上有哪些。
fn service_javas(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let root = game_root(h, req);
    let id = field(req, "id").unwrap_or("");
    let (need, component) = if id.is_empty() {
        (0u32, None)
    } else {
        match resolve_local(&root, id) {
            Some(v) => java::required(&v, id),
            None => (0, None),
        }
    };
    let list = java::scan(h);
    let picked = if need > 0 { java::pick(&list, need) } else { None };
    Ok(serde_json::to_vec(&json!({
        "ok": true,
        "id": id,
        "required": need,
        "component": component,
        "picked": picked.as_ref().map(|j| j.to_json()),
        "items": list.iter().map(|j| j.to_json()).collect::<Vec<_>>(),
    }))
    .unwrap_or_default())
}

/// `miy.mc.java.install` —— 只下 Java，不启动游戏（界面的「安装 Java」按钮用）。
fn service_java_install(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let root = game_root(h, req);
    let id = field(req, "id").unwrap_or("");
    let mut need = 0u32;
    let mut component: Option<String> = None;
    if !id.is_empty() {
        if let Some(v) = resolve_local(&root, id) {
            let (n, c) = java::required(&v, id);
            need = n;
            component = c;
        }
    }
    if need == 0 {
        need = req.get("major").and_then(Value::as_u64).unwrap_or(0) as u32;
    }
    if need == 0 {
        return Err("缺 id 或 major，不知道该装哪个 Java".into());
    }
    let mut notes = Vec::new();
    let j = java::install(h, need, component.as_deref(), &mut notes)?;
    Ok(serde_json::to_vec(&json!({
        "ok": true,
        "java": j.to_json(),
        "notes": notes,
    }))
    .unwrap_or_default())
}

// ---- 每个版本自己的偏好（内存）与文件夹快捷入口 ---------------------------

/// 版本目录里的小配置。跟着版本走：版本被删/被搬，设置也跟着走。
fn instance_file(version_dir: &str) -> String {
    format!("{version_dir}\\miy.json")
}

fn instance_doc(version_dir: &str) -> Value {
    std::fs::read_to_string(instance_file(version_dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}))
}

fn instance_get(version_dir: &str, key: &str) -> Option<Value> {
    instance_doc(version_dir).get(key).cloned()
}

fn instance_str(version_dir: &str, key: &str) -> String {
    instance_get(version_dir, key)
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn instance_bool(version_dir: &str, key: &str) -> bool {
    instance_get(version_dir, key)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 写一个键；`None` 表示删掉它（回到跟随全局）。
fn instance_set(version_dir: &str, key: &str, value: Option<Value>) -> Result<(), String> {
    let path = instance_file(version_dir);
    let mut doc = instance_doc(version_dir);
    let obj = doc.as_object_mut().ok_or("版本配置不是对象")?;
    match value {
        Some(v) => {
            obj.insert(key.to_string(), v);
        }
        None => {
            obj.remove(key);
        }
    }
    if obj.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("无法写入 {path}：{e}"))
}

fn instance_memory(version_dir: &str) -> Option<u64> {
    instance_get(version_dir, "memory").and_then(|m| m.as_u64())
}

fn set_instance_memory(version_dir: &str, memory: Option<u64>) -> Result<(), String> {
    instance_set(version_dir, "memory", memory.map(|m| json!(m)))
}

/// 版本级高级选项：内存、JVM 参数、游戏参数、启动前命令、禁止更新模组。
/// 不带 `set` 就是读；带 `set` 就按字段写（null 表示跟随全局）。
fn service_vsettings(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let root = game_root(h, req);
    let id = field(req, "id").unwrap_or("").trim().to_string();
    if id.is_empty() {
        return Err("缺少版本名".into());
    }
    let dir = std::path::Path::new(&root).join("versions").join(&id);
    if !dir.is_dir() {
        return Err(format!("版本 {id} 不存在"));
    }
    let dir_s = dir.to_string_lossy().into_owned();
    if let Some(set) = req.get("set").and_then(Value::as_object) {
        for (k, v) in set {
            match k.as_str() {
                "memory" => {
                    let m = v.as_u64().filter(|m| *m > 0);
                    instance_set(&dir_s, "memory", m.map(|m| json!(m)))?;
                }
                "jvmArgs" | "gameArgs" | "preLaunch" => {
                    let text = v.as_str().unwrap_or("").trim().to_string();
                    if text.is_empty() {
                        instance_set(&dir_s, k, None)?;
                    } else {
                        instance_set(&dir_s, k, Some(json!(text)))?;
                    }
                }
                "noModUpdate" => {
                    let on = v.as_bool().unwrap_or(false);
                    if on {
                        instance_set(&dir_s, k, Some(json!(true)))?;
                    } else {
                        instance_set(&dir_s, k, None)?;
                    }
                }
                other => return Err(format!("未知的版本设置项 {other}")),
            }
        }
    }
    Ok(serde_json::to_vec(&json!({
        "ok": true,
        "id": id,
        "memory": instance_memory(&dir_s),
        "jvmArgs": instance_str(&dir_s, "jvmArgs"),
        "gameArgs": instance_str(&dir_s, "gameArgs"),
        "preLaunch": instance_str(&dir_s, "preLaunch"),
        "noModUpdate": instance_bool(&dir_s, "noModUpdate"),
    }))
    .unwrap_or_default())
}

/// 读（不带 memory）或写（带 memory，0 表示恢复默认）某个版本的内存分配。
fn service_memory(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let root = game_root(h, req);
    let id = field(req, "id").unwrap_or("").trim().to_string();
    if id.is_empty() {
        return Err("缺少版本名".into());
    }
    let dir = format!("{root}\\versions\\{id}");
    // 数字和字符串都收：界面上填的是字符串，探针里传的是数字。
    let want = req.get("memory").and_then(|v| match v {
        Value::Number(n) => n.as_u64(),
        Value::String(t) => t.trim().parse::<u64>().ok(),
        _ => None,
    });
    match want {
        Some(0) => {
            set_instance_memory(&dir, None)?;
            Ok(serde_json::to_vec(&json!({ "ok": true, "memory": Value::Null })).unwrap_or_default())
        }
        Some(m) => {
            let m = m.clamp(512, 65536);
            set_instance_memory(&dir, Some(m))?;
            Ok(serde_json::to_vec(&json!({ "ok": true, "memory": m })).unwrap_or_default())
        }
        None => {
            let snap = settings_snapshot(h);
            Ok(serde_json::to_vec(&json!({
                "memory": instance_memory(&dir),
                "default": setting_u64(&snap, "miy.mc.memory", 2048),
            }))
            .unwrap_or_default())
        }
    }
}

/// 打开某个版本的模组/存档/资源包/光影文件夹，没有就先建出来。
fn service_folder(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let root = game_root(h, req);
    let id = field(req, "id").unwrap_or("").trim().to_string();
    if id.is_empty() {
        return Err("缺少版本名".into());
    }
    let kind = field(req, "kind").unwrap_or("").trim().to_string();
    let sub = match kind.as_str() {
        "mods" => "mods",
        "saves" => "saves",
        "resourcepacks" => "resourcepacks",
        "shaderpacks" => "shaderpacks",
        "version" => "",
        _ => return Err("未知的文件夹类型".into()),
    };
    let dir = if sub.is_empty() {
        format!("{root}\\versions\\{id}")
    } else {
        format!("{root}\\versions\\{id}\\{sub}")
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建 {dir}：{e}"))?;
    let payload = serde_json::to_vec(&json!({ "path": dir })).unwrap_or_default();
    let reply = unsafe { h.call_json("miy.app.open", &payload) }.unwrap_or_default();
    let ok = serde_json::from_slice::<Value>(&reply)
        .ok()
        .and_then(|v| v.get("ok").and_then(|b| b.as_bool()))
        .unwrap_or(true);
    Ok(serde_json::to_vec(&json!({ "ok": ok, "path": dir })).unwrap_or_default())
}

/// 正在跑的游戏进程（同一时间只认一个）。
static RUNNING: Mutex<Option<(u32, String)>> = Mutex::new(None);

fn set_running(pid: Option<(u32, String)>) {
    if let Ok(mut g) = RUNNING.lock() {
        *g = pid;
    }
}

fn running_now() -> Option<(u32, String)> {
    RUNNING.lock().ok().and_then(|g| g.clone())
}

/// 启动前先告诉界面「要开始了」，失败也要说一声，别让动画一直转。
// ---- 整合包导出 -----------------------------------------------------------

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *slot = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for b in data {
        crc = table[((crc ^ *b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// 极简 zip 写入器：只做 deflate，够导出整合包用。
struct ZipOut {
    data: Vec<u8>,
    central: Vec<u8>,
    count: u16,
}

impl ZipOut {
    fn new() -> Self {
        Self { data: Vec::new(), central: Vec::new(), count: 0 }
    }

    fn add(&mut self, name: &str, bytes: &[u8]) -> Result<(), String> {
        use std::io::Write;
        let crc = crc32(bytes);
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(bytes).map_err(|e| e.to_string())?;
        let comp = enc.finish().map_err(|e| e.to_string())?;
        let method: u16 = 8;
        let offset = self.data.len() as u32;
        let name_b = name.as_bytes();
        // 名字里有非 ASCII 就置 UTF-8 标志位（bit 11）。不置的话 Windows 资源管理器
        // 会按本地代码页解码，中文世界名解压出来就是乱码。
        let flags: u16 = if name_b.is_ascii() { 0 } else { 0x0800 };
        let mut local = Vec::new();
        local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes());
        local.extend_from_slice(&flags.to_le_bytes());
        local.extend_from_slice(&method.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        local.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        local.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(name_b);
        self.data.extend_from_slice(&local);
        self.data.extend_from_slice(&comp);

        let mut cen = Vec::new();
        cen.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        cen.extend_from_slice(&20u16.to_le_bytes());
        cen.extend_from_slice(&20u16.to_le_bytes());
        cen.extend_from_slice(&flags.to_le_bytes());
        cen.extend_from_slice(&method.to_le_bytes());
        cen.extend_from_slice(&0u16.to_le_bytes());
        cen.extend_from_slice(&0u16.to_le_bytes());
        cen.extend_from_slice(&crc.to_le_bytes());
        cen.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        cen.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        cen.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
        cen.extend_from_slice(&0u16.to_le_bytes());
        cen.extend_from_slice(&0u16.to_le_bytes());
        cen.extend_from_slice(&0u16.to_le_bytes());
        cen.extend_from_slice(&0u16.to_le_bytes());
        cen.extend_from_slice(&0u32.to_le_bytes());
        cen.extend_from_slice(&offset.to_le_bytes());
        cen.extend_from_slice(name_b);
        self.central.extend_from_slice(&cen);
        self.count += 1;
        Ok(())
    }

    fn finish(mut self) -> Vec<u8> {
        let cen_off = self.data.len() as u32;
        let cen_len = self.central.len() as u32;
        self.data.extend_from_slice(&self.central);
        let mut end = Vec::new();
        end.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        end.extend_from_slice(&self.count.to_le_bytes());
        end.extend_from_slice(&self.count.to_le_bytes());
        end.extend_from_slice(&cen_len.to_le_bytes());
        end.extend_from_slice(&cen_off.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        self.data.extend_from_slice(&end);
        self.data
    }
}

fn walk_files(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<(String, std::path::PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            walk_files(&path, base, out);
        } else if let Ok(rel) = path.strip_prefix(base) {
            out.push((rel.to_string_lossy().replace('\\', "/"), path));
        }
    }
}

/// 导出前先列一遍候选文件：让用户自己勾选要带走的模组 / 资源包 / 光影。
fn service_export_list(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let root = game_root(h, req);
    let id = field(req, "id").unwrap_or("").trim().to_string();
    if id.is_empty() {
        return Err("缺少版本名".into());
    }
    let dir = std::path::Path::new(&root).join("versions").join(&id);
    if !dir.is_dir() {
        return Err(format!("版本 {id} 不存在"));
    }
    let list = |sub: &str| -> Vec<Value> {
        let base = dir.join(sub);
        let mut files = Vec::new();
        walk_files(&base, &base, &mut files);
        files.sort_by_key(|(rel, _)| rel.to_lowercase());
        files
            .into_iter()
            .filter_map(|(rel, path)| {
                let size = std::fs::metadata(&path).ok()?.len();
                Some(json!({ "file": rel, "size": size }))
            })
            .collect()
    };
    // 存档是「一个世界一个目录」，勾选单位是整个存档，不是里面的 level.dat。
    let mut saves: Vec<Value> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir.join("saves")) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let mut files = Vec::new();
            walk_files(&p, &p, &mut files);
            let size: u64 = files
                .iter()
                .filter_map(|(_, f)| std::fs::metadata(f).ok().map(|m| m.len()))
                .sum();
            saves.push(json!({
                "file": e.file_name().to_string_lossy(),
                "size": size,
                "files": files.len(),
            }));
        }
    }
    saves.sort_by(|a, b| {
        a["file"]
            .as_str()
            .unwrap_or("")
            .cmp(b["file"].as_str().unwrap_or(""))
    });
    Ok(serde_json::to_vec(&json!({
        "ok": true,
        "id": id,
        "mods": list("mods"),
        "resourcepacks": list("resourcepacks"),
        "shaderpacks": list("shaderpacks"),
        "saves": saves,
        "hasConfig": dir.join("config").is_dir(),
        "hasOptions": dir.join("options.txt").is_file(),
        "hasSaves": dir.join("saves").is_dir(),
    }))
    .unwrap_or_default())
}

/// 把一个子目录按勾选结果打进 zip；`only` 为 `None` 表示全导。
fn export_dir(
    zip: &mut ZipOut,
    dir: &std::path::Path,
    sub: &str,
    only: Option<&Vec<String>>,
    count: &mut u32,
) -> Result<(), String> {
    let base = dir.join(sub);
    let mut files = Vec::new();
    walk_files(&base, &base, &mut files);
    files.sort_by_key(|(rel, _)| rel.clone());
    for (rel, path) in files {
        if let Some(sel) = only {
            if !sel.iter().any(|s| s == &rel) {
                continue;
            }
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        zip.add(&format!("{sub}/{rel}"), &bytes)?;
        *count += 1;
    }
    Ok(())
}

/// 把版本导出成整合包 zip：模组 + 资源包 + 光影 + 配置 + 选项 + 一份清单。
///
/// `mods` / `resourcepacks` / `shaderpacks` 传数组就是「只导出这些」（相对路径），
/// 不传就是全导，传空数组就是不导 —— 和 PCL2 导出时的勾选一致。
fn service_export(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let root = game_root(h, req);
    let id = field(req, "id").unwrap_or("").trim().to_string();
    if id.is_empty() {
        return Err("缺少版本名".into());
    }
    let dir = std::path::Path::new(&root).join("versions").join(&id);
    if !dir.is_dir() {
        return Err(format!("版本 {id} 不存在"));
    }
    let pick = |key: &str| -> Option<Vec<String>> {
        match req.get(key) {
            Some(Value::Array(a)) => Some(
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.replace('\\', "/"))
                    .collect(),
            ),
            _ => None,
        }
    };
    let want_mods = pick("mods");
    let want_rp = pick("resourcepacks");
    let want_sp = pick("shaderpacks");
    let want_config = req.get("config").and_then(Value::as_bool).unwrap_or(true);
    let want_options = req.get("options").and_then(Value::as_bool).unwrap_or(true);
    // 存档：数组 = 只导这几个世界，true = 全导，缺省 = 不导。
    let (want_saves, saves_only) = match req.get("saves") {
        Some(Value::Array(a)) => (
            true,
            Some(
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.replace('\\', "/"))
                    .collect::<Vec<String>>(),
            ),
        ),
        Some(Value::Bool(b)) => (*b, None),
        _ => (false, None),
    };

    let (task, _cancel) = task_new("export", &format!("导出 {id}"), 1);
    let mut zip = ZipOut::new();
    let mut mods_meta: Vec<Value> = Vec::new();
    let mut count = 0u32;

    // 模组单独走一遍：清单里要记 sha1，方便以后按哈希重新下载。
    {
        let base = dir.join("mods");
        let mut files = Vec::new();
        walk_files(&base, &base, &mut files);
        files.sort_by_key(|(rel, _)| rel.clone());
        for (rel, path) in files {
            if let Some(sel) = &want_mods {
                if !sel.iter().any(|s| s == &rel) {
                    continue;
                }
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let sha = crate::java::sha1_hex(&bytes);
            mods_meta.push(json!({
                "file": rel,
                "sha1": sha,
                "size": bytes.len(),
            }));
            zip.add(&format!("mods/{rel}"), &bytes)?;
            count += 1;
        }
    }
    export_dir(&mut zip, &dir, "resourcepacks", want_rp.as_ref(), &mut count)?;
    export_dir(&mut zip, &dir, "shaderpacks", want_sp.as_ref(), &mut count)?;
    if want_config {
        export_dir(&mut zip, &dir, "config", None, &mut count)?;
    }
    if want_options {
        let opt = dir.join("options.txt");
        if let Ok(bytes) = std::fs::read(&opt) {
            zip.add("options.txt", &bytes)?;
            count += 1;
        }
    }
    if want_saves {
        let base = dir.join("saves");
        let mut worlds: Vec<String> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten() {
                if e.path().is_dir() {
                    worlds.push(e.file_name().to_string_lossy().to_string());
                }
            }
        }
        worlds.sort();
        for w in worlds {
            if let Some(sel) = &saves_only {
                if !sel.iter().any(|s| s == &w) {
                    continue;
                }
            }
            let wdir = base.join(&w);
            let mut files = Vec::new();
            walk_files(&wdir, &wdir, &mut files);
            files.sort_by_key(|(rel, _)| rel.clone());
            for (rel, path) in files {
                let Ok(bytes) = std::fs::read(&path) else { continue };
                zip.add(&format!("saves/{w}/{rel}"), &bytes)?;
                count += 1;
            }
        }
    }

    let info = service_verinfo(h, req).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let mc = info
        .as_ref()
        .and_then(|v| v.get("mc").and_then(Value::as_str))
        .unwrap_or(&id)
        .to_string();
    let loader = info
        .as_ref()
        .and_then(|v| v.get("loader").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let manifest = json!({
        "format": "miylauncher-modpack/1",
        "name": id,
        "mc": mc,
        "loader": loader,
        "created": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "mods": mods_meta,
        "files": count,
    });
    zip.add(
        "modpack.json",
        &serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
    )?;
    let readme = format!(
        "MIYLauncher 整合包\n\n版本：{id}\nMinecraft：{mc}\n加载器：{}\n\n把 mods 与 config 放进对应版本的目录即可。\n",
        if loader.is_empty() { "原版" } else { &loader }
    );
    zip.add("README.txt", readme.as_bytes())?;

    let out_dir = std::path::Path::new(&root).join("exports");
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("无法创建导出目录：{e}"))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let out = out_dir.join(format!("{id}-{stamp}.zip"));
    std::fs::write(&out, zip.finish()).map_err(|e| format!("无法写入整合包：{e}"))?;
    task_finish(task, true, "");
    Ok(serde_json::to_vec(&json!({
        "ok": true,
        "path": out.to_string_lossy(),
        "files": count,
        "mods": mods_meta.len(),
    }))
    .unwrap_or_default())
}

// ---- 已装模组的图标 / 详情 / 删除 -----------------------------------------

/// sha1 → 项目信息（标题、图标、项目 id）。查一次就记住，落盘复用。
static MOD_META: Mutex<Option<std::collections::HashMap<String, Value>>> = Mutex::new(None);

fn mod_meta_file(h: &MiyHostV1) -> Option<std::path::PathBuf> {
    let info = unsafe { h.call_json("miy.app.info", b"{}") }?;
    let v: Value = serde_json::from_slice(&info).ok()?;
    let root = v.get("dataRoot").and_then(Value::as_str)?;
    Some(std::path::Path::new(root).join("cache").join("mod_meta.json"))
}

fn mod_meta_load(h: &MiyHostV1) -> std::collections::HashMap<String, Value> {
    if let Ok(g) = MOD_META.lock() {
        if let Some(m) = g.as_ref() {
            return m.clone();
        }
    }
    let mut map = std::collections::HashMap::new();
    if let Some(file) = mod_meta_file(h) {
        if let Ok(bytes) = std::fs::read(&file) {
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(obj) = v.as_object() {
                    for (k, val) in obj {
                        map.insert(k.clone(), val.clone());
                    }
                }
            }
        }
    }
    if let Ok(mut g) = MOD_META.lock() {
        *g = Some(map.clone());
    }
    map
}

fn mod_meta_save(h: &MiyHostV1, map: &std::collections::HashMap<String, Value>) {
    if let Ok(mut g) = MOD_META.lock() {
        *g = Some(map.clone());
    }
    let Some(file) = mod_meta_file(h) else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let obj: serde_json::Map<String, Value> = map
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if let Ok(bytes) = serde_json::to_vec(&Value::Object(obj)) {
        let _ = std::fs::write(&file, bytes);
    }
}

/// 用 sha1 反查 Modrinth：拿到项目 id 后再取标题、图标、简介。
fn mod_lookup(h: &MiyHostV1, sha1: &str) -> Option<Value> {
    let modrinth = modrinth_base();
    let url = format!("{modrinth}/version_file/{sha1}?algorithm=sha1");
    let v = fetch_json(h, &url).ok()?;
    let project = v.get("project_id").and_then(Value::as_str)?.to_string();
    let version = v
        .get("version_number")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let proj = fetch_json(h, &format!("{modrinth}/project/{project}")).ok()?;
    Some(json!({
        "project": project,
        "title": proj.get("title").cloned().unwrap_or(Value::Null),
        "desc": proj.get("description").cloned().unwrap_or(Value::Null),
        "icon": proj.get("icon_url").cloned().unwrap_or(Value::Null),
        "slug": proj.get("slug").cloned().unwrap_or(Value::Null),
        "downloads": proj.get("downloads").cloned().unwrap_or(Value::Null),
        "version": version,
    }))
}

fn sha1_of_file(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(crate::java::sha1_hex(&bytes))
}

/// 给一批已装模组补图标与标题（并行查，查过的走缓存）。
fn service_modicons(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let root = game_root(h, req);
    let id = field(req, "id").unwrap_or("").trim().to_string();
    if id.is_empty() {
        return Err("缺少版本名".into());
    }
    let dir = std::path::Path::new(&root).join("versions").join(&id).join("mods");
    let mut cache = mod_meta_load(h);
    let mut files: Vec<(String, String)> = Vec::new(); // (file, sha1)
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !(name.ends_with(".jar") || name.ends_with(".jar.disabled")) {
                continue;
            }
            if let Some(sha) = sha1_of_file(&e.path()) {
                files.push((name, sha));
            }
        }
    }
    let todo: Vec<(String, String)> = files
        .iter()
        .filter(|(_, sha)| !cache.contains_key(sha))
        .cloned()
        .collect();
    if !todo.is_empty() {
        let host = *h;
        let raw = Box::into_raw(Box::new(host)) as usize;
        let queue = std::sync::Arc::new(std::sync::Mutex::new(todo));
        let done = std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::<String, Value>::new(),
        ));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let q = queue.clone();
            let d = done.clone();
            handles.push(std::thread::spawn(move || {
                let host = unsafe { &*(raw as *const MiyHostV1) };
                loop {
                    let item = { q.lock().ok().and_then(|mut g| g.pop()) };
                    let Some((_, sha)) = item else { break };
                    let meta = mod_lookup(host, &sha).unwrap_or(Value::Null);
                    if let Ok(mut g) = d.lock() {
                        g.insert(sha, meta);
                    }
                }
            }));
        }
        for t in handles {
            let _ = t.join();
        }
        if let Ok(g) = done.lock() {
            for (k, v) in g.iter() {
                cache.insert(k.clone(), v.clone());
            }
        }
        mod_meta_save(h, &cache);
    }
    let mut out = Vec::new();
    for (file, sha) in files {
        let meta = cache.get(&sha).cloned().unwrap_or(Value::Null);
        out.push(json!({
            "file": file,
            "sha1": sha,
            "meta": meta,
        }));
    }
    Ok(serde_json::to_vec(&json!({ "ok": true, "items": out })).unwrap_or_default())
}

/// 删掉一个已装模组（.jar 或 .jar.disabled 都认）。
fn service_modremove(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let root = game_root(h, req);
    let id = field(req, "id").unwrap_or("").trim().to_string();
    let file = field(req, "file").unwrap_or("").trim().to_string();
    if id.is_empty() || file.is_empty() {
        return Err("缺少版本名或文件名".into());
    }
    if file.contains('\\') || file.contains('/') || file.contains("..") {
        return Err("文件名不合法".into());
    }
    let path = std::path::Path::new(&root)
        .join("versions")
        .join(&id)
        .join("mods")
        .join(&file);
    if !path.is_file() {
        return Err(format!("找不到 {file}"));
    }
    std::fs::remove_file(&path).map_err(|e| format!("无法删除 {file}：{e}"))?;
    Ok(serde_json::to_vec(&json!({ "ok": true, "file": file })).unwrap_or_default())
}

/// 现在有没有游戏在跑。
fn service_running(_h: &MiyHostV1, _req: &Value) -> Result<Vec<u8>, String> {
    Ok(match running_now() {
        Some((pid, id)) => serde_json::to_vec(&json!({ "ok": true, "pid": pid, "id": id }))
            .unwrap_or_default(),
        None => serde_json::to_vec(&json!({ "ok": false })).unwrap_or_default(),
    })
}

/// 启动过程中用户点了「取消启动」：置位后启动流程在下一个检查点放弃。
/// 启动要读清单、补资源、挑 Java，慢的时候十几秒，这期间得能反悔。
static LAUNCH_CANCEL: AtomicBool = AtomicBool::new(false);

fn launch_cancelled() -> bool {
    LAUNCH_CANCEL.load(Ordering::SeqCst)
}

fn launch_cancelled_payload(id: &str, root: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "ok": false,
        "code": "cancelled",
        "error": "已取消启动",
        "id": id,
        "root": root
    }))
    .unwrap_or_default()
}

/// 取消启动：还没起来就让启动流程自己退出，已经起来了就直接结束进程。
fn service_launch_cancel(_h: &MiyHostV1, _req: &Value) -> Result<Vec<u8>, String> {
    LAUNCH_CANCEL.store(true, Ordering::SeqCst);
    let killed = if let Some((pid, id)) = running_now() {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
        set_running(None);
        Some(id)
    } else {
        None
    };
    Ok(serde_json::to_vec(&json!({ "ok": true, "killed": killed })).unwrap_or_default())
}

/// 关掉正在跑的游戏（连子进程一起）。
fn service_kill(_h: &MiyHostV1, _req: &Value) -> Result<Vec<u8>, String> {
    let Some((pid, id)) = running_now() else {
        return Ok(serde_json::to_vec(&json!({ "ok": false, "error": "没有正在运行的游戏" }))
            .unwrap_or_default());
    };
    let out = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            set_running(None);
            Ok(serde_json::to_vec(&json!({ "ok": true, "pid": pid, "id": id })).unwrap_or_default())
        }
        Ok(o) => Err(format!(
            "结束进程失败：{}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Err(format!("结束进程失败：{e}")),
    }
}

fn service_launch(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let id = field(req, "id").unwrap_or("").to_string();
    // 每次启动都从「没被取消」开始。
    LAUNCH_CANCEL.store(false, Ordering::SeqCst);
    if !id.is_empty() {
        let payload = serde_json::to_vec(&json!({ "id": id })).unwrap_or_default();
        unsafe { h.emit_str("mc.launching", &payload) };
    }
    let out = service_launch_inner(h, req);
    if let Err(e) = &out {
        let payload = serde_json::to_vec(&json!({ "id": id, "error": e })).unwrap_or_default();
        unsafe { h.emit_str("mc.launch.fail", &payload) };
    }
    out
}

fn service_launch_inner(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    // `id` 是磁盘上的版本目录名（可能带加载器后缀，和 MC 版本号不一定同名）。
    let id = field(req, "id").ok_or("缺 id")?;
    let root = game_root(h, req);
    if launch_cancelled() {
        return Ok(launch_cancelled_payload(id, &root));
    }
    // 本地优先：装好的版本必须能离线启动，不能每次去问远端清单。
    let v = match resolve_local_multi(&all_roots(h), id) {
        Some(v) => v,
        None => {
            let man = manifest_for(h, id);
            resolve(h, &man, id, 0)?
        }
    };

    // 没指定就用当前身份：`miy.accounts.identity` 已经替我们决定是正版还是离线，
    // 而且离线身份的名字/UUID 是用户自己配的（以前这里硬写 "Player" + 全 0 UUID，
    // 导致离线时进不了自己改过名的世界）。
    let mut user = field(req, "username").map(str::to_string);
    let mut uuid = String::new();
    let mut token = String::from("0");
    let mut user_type = String::from("legacy");
    let mut online = false;

    if let Ok(id) = call_json(h, "miy.accounts.identity", &json!({})) {
        let typ = id.get("type").and_then(Value::as_str).unwrap_or("offline");
        if let Some(n) = id.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            user.get_or_insert_with(|| n.to_string());
        }
        if let Some(u) = id.get("uuid").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            uuid = u.replace('-', "");
        }
        if typ == "msa" {
            if let Some(t) = id.get("token").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                token = t.to_string();
                user_type = id
                    .get("user_type")
                    .and_then(Value::as_str)
                    .unwrap_or("msa")
                    .to_string();
                online = true;
            }
        }
    }
    let user = user.unwrap_or_else(|| "Player".to_string());
    if uuid.is_empty() {
        uuid = "00000000000000000000000000000000".to_string();
    }

    // 版本隔离：游戏目录就是这个版本自己的文件夹，mods / saves / config 都在里面，
    // 和别的版本互不相通；assets 与 libraries 仍然是整份共享的。
    let version_dir = format!("{root}\\versions\\{id}");
    let _ = std::fs::create_dir_all(&version_dir);
    // 补齐版本自己的标准目录：从别处（比如 PCL2）加进来的版本也能直接玩，
    // 缺哪个补哪个，已有的不动。
    for sub in ["mods", "saves", "config", "logs", "resourcepacks", "shaderpacks", "crash-reports"] {
        let p = format!("{version_dir}\\{sub}");
        if !std::path::Path::new(&p).is_dir() {
            let _ = std::fs::create_dir_all(&p);
        }
    }
    // Natives 目录跟着版本走（PCL2 就是 `<版本目录>\<版本名>-natives\`）。
    // 版本 JSON 的 `arguments.jvm` 会把 `${natives_directory}` 展开到这里，
    // 里面再分 java / jna / lwjgl / netty 四个临时目录，缺一个原生库就加载不了。
    let natives = format!("{version_dir}\\{id}-natives");

    // 老版本的原生库（带 `natives.windows` 的库）不进类路径 —— PCL2 也是
    // `If Library.IsNatives Then Continue For`：它们要解压成 dll 交给
    // `-Djava.library.path`，塞进类路径只会让 JVM 去里面找类。
    let native_jars = natives_jars(&root, &v);

    // 类路径：所有规则允许的运行库 + 客户端 jar
    let mut cp: Vec<String> = Vec::new();
    if let Some(arr) = v.get("libraries").and_then(Value::as_array) {
        for lib in arr {
            if !allows(lib.get("rules")) {
                continue;
            }
            if lib.get("natives").and_then(|n| n.get("windows")).is_some() {
                continue;
            }
            if let Some((rel, _, _, _)) = lib_entry(lib) {
                cp.push(format!("{root}\\libraries\\{}", rel.replace('/', "\\")));
            }
        }
    }
    // OptiFine 要在客户端 jar 前面一位（PCL2 也是插到倒数第二位）：
    // 它得赶在游戏主类之前把补丁类挂上去，排到 jar 后面就轮不到它了。
    if let Some(pos) = cp.iter().position(|p| p.to_lowercase().contains("optifine")) {
        let optifine = cp.remove(pos);
        cp.push(optifine);
    }
    // 客户端 jar：加载器版本（Fabric/Quilt）自己没有 jar，沿 inheritsFrom 找原版的。
    let client_jar = client_jar_for(&all_roots(h), id, &v)
        .ok_or_else(|| "缺少客户端文件，请先安装该版本。".to_string())?;
    cp.push(client_jar.clone());

    // 启动前完整性检查：缺文件不许启动。
    // 少一个资源对象，MC 会在 TextureManager / LinkFSProvider 里抛
    // `NoSuchFileException`，表现成"游戏刚打开就闪退"，用户完全无从下手。
    // 与其让他自己猜，不如在这里拦住并告诉他还缺多少。
    // 判「资源齐不齐」必须用 `assets` 字段：游戏拿它当 `--assetIndex`。
    // 加载器版本里它和 `assetIndex.id` 不一定相等，查错索引等于没查。
    let assets_id = v
        .get("assets")
        .and_then(Value::as_str)
        .or_else(|| {
            v.get("assetIndex")
                .and_then(|i| i.get("id"))
                .and_then(Value::as_str)
        })
        .unwrap_or("legacy")
        .to_string();

    let index_path = format!("{root}\\assets\\indexes\\{assets_id}.json");
    let index: Option<Value> = std::fs::read_to_string(&index_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let assets_known = index.is_some();
    let (mut missing_libs, mut missing_assets) =
        missing_counts(&root, &cp, &native_jars, index.as_ref());
    if launch_cancelled() {
        return Ok(launch_cancelled_payload(id, &root));
    }

    // P1-5：缺文件先自己补，别一上来就甩「请补齐后再启动」。
    // PCL2 的「预启动处理」就是这个思路 —— 玩家点启动，启动器该把缺的东西
    // 悄悄补上；只有补不齐（比如镜像挂了）才值得打断他。
    if (missing_libs > 0 || missing_assets > 0) && assets_known {
        let mut fix: Vec<Item> = Vec::new();
        let (libs, extra) = plan(&v, id);
        for it in libs.into_iter().chain(extra) {
            if !file_ok(&root, &it) {
                fix.push(it);
            }
        }
        if let Some(ix) = index.as_ref() {
            for it in asset_items(ix) {
                if !file_ok(&root, &it) {
                    fix.push(it);
                }
            }
        }
        if !fix.is_empty() {
            unsafe {
                h.log_str(
                    MIY_LOG_INFO,
                    &format!("启动前自检：缺 {} 个文件，正在静默补齐", fix.len()),
                );
            }
            let (task, cancel) =
                task_new("repair", &format!("启动前补齐 {id}"), fix.len() as u64);
            let got = fetch_all(
                h,
                &root,
                &fix,
                concurrency(h),
                (fix.len() / 50).max(1),
                Some(task),
                Some(&cancel),
            );
            if got.cancelled {
                return Ok(launch_cancelled_payload(id, &root));
            }
            let (l2, a2) = missing_counts(&root, &cp, &native_jars, index.as_ref());
            missing_libs = l2;
            missing_assets = a2;
            if missing_libs == 0 && missing_assets == 0 {
                unsafe {
                    h.log_str(
                        MIY_LOG_INFO,
                        &format!("启动前补齐完成：{} 个文件", fix.len()),
                    );
                }
            }
        }
    }

    if missing_libs > 0 || missing_assets > 0 || !assets_known {
        let mut parts: Vec<String> = Vec::new();
        if missing_libs > 0 {
            parts.push(format!("缺少 {missing_libs} 个运行库文件"));
        }
        if !assets_known {
            parts.push("资源索引尚未下载".to_string());
        } else if missing_assets > 0 {
            parts.push(format!("有 {missing_assets} 个资源文件缺失或损坏"));
        }
        let error = format!("{}，请补齐后再启动。", parts.join("；"));
        return Ok(serde_json::to_vec(&json!({
            "ok": false,
            "code": "incomplete",
            "error": error,
            "missing": { "libs": missing_libs, "assets": missing_assets },
            "id": id,
            "root": root
        }))
        .unwrap_or_default());
    }

    let classpath = cp.join(";");

    // natives 目录要提前建好：现代版本的 `arguments.jvm` 会把 LWJGL / JNA 的
    // 临时解压目录指到这几个子目录里，路径不存在时原生库加载会失败。
    for sub in [
        natives.clone(),
        format!("{natives}\\java"),
        format!("{natives}\\jna"),
        format!("{natives}\\lwjgl"),
        format!("{natives}\\netty"),
    ] {
        let _ = std::fs::create_dir_all(&sub);
    }

    // 和 PCL2 的「预启动处理」一样：进游戏之前先把语言 / 窗口写进 options.txt。
    // 不写的话，新装好的版本第一次启动就是英文界面；玩家自己去改语言又会触发
    // 一次全量资源重载 —— 资源没下全的时候，就崩在重载里。
    let snap = settings_snapshot(h);
    let mut notes = sync_options_txt(&root, id, &v, &snap);

    // 老版本的原生库要解压成 dll，`-Djava.library.path` 才找得到。
    // 1.19+ 的 natives 是普通运行库条目（由 LWJGL 自己在运行时解压），
    // 这里拿到的是空列表，函数直接返回。
    if let Err(err) = extract_natives(&native_jars, &natives, &mut notes) {
        return Ok(serde_json::to_vec(&json!({
            "ok": false,
            "code": "incomplete",
            "error": err,
            "id": id,
            "root": root
        }))
        .unwrap_or_default());
    }

    // 1.6 以前的索引是虚拟的：游戏按原始路径去 `assets/virtual/legacy` 里读，
    // 而不是 `objects/<hash>`。PCL2 下载时就直接写进虚拟目录，我们启动前摆好。
    match sync_virtual_assets(&root, &version_dir, &assets_id) {
        Ok(copied) if copied > 0 => notes.push(format!("已准备 {copied} 个旧版资源文件。")),
        Ok(_) => {}
        Err(err) => {
            return Ok(serde_json::to_vec(&json!({
                "ok": false,
                "code": "incomplete",
                "error": err,
                "id": id,
                "root": root
            }))
            .unwrap_or_default());
        }
    }

    // Java 环境：按版本 JSON 的要求挑一个，机器上真没有就下 Mojang 的运行时。
    // 以前这里只是「设置里的路径 > JAVA_HOME > PATH 里的 java.exe」一条直线，
    // 后果是：没装 Java 的人点启动只会看到一句"无法启动 Java 进程"；
    // 装了多个 Java 的人则经常拿到 8 或 17 去跑 26.x，游戏窗口一闪就没。
    let (need_java, java_component) = java::required(&v, id);
    if launch_cancelled() {
        return Ok(launch_cancelled_payload(id, &root));
    }
    let allow_java_download = setting_bool(&snap, "miy.mc.javaAuto", true);
    let mut java_notes: Vec<String> = Vec::new();
    let java_pick = match java::ensure(
        h,
        need_java,
        java_component.as_deref(),
        allow_java_download,
        &mut java_notes,
    ) {
        Ok(j) => j,
        Err(err) => {
            return Ok(serde_json::to_vec(&json!({
                "ok": false,
                "code": "nojava",
                "error": format!("该版本需要 Java {need_java}：{err}"),
                "id": id,
                "root": root,
                "javaNeed": need_java,
                "notes": notes
            }))
            .unwrap_or_default())
        }
    };
    notes.extend(java_notes);
    // 每次启动都把用的是哪个 Java 写一行：出问题时这是第一个该看的证据。
    notes.push(format!(
        "正在使用 Java {}（{}）。",
        java_pick.major, java_source_label(&java_pick.source)
    ));

    // 游戏自己的临时文件也别落到 `%TEMP%`：`java.io.tmpdir` 指到版本目录里，
    // `-XX:-UsePerfData` 顺手关掉 `%TEMP%\hsperfdata_*` 这堆按 pid 命名的小目录。
    let game_tmp = format!("{version_dir}\\temp");
    let _ = std::fs::create_dir_all(&game_tmp);

    for note in &notes {
        let payload = serde_json::to_vec(&json!({ "line": note.clone() })).unwrap_or_default();
        unsafe { h.emit_str("mc.log", &payload) };
    }

    let width = setting_u64(&snap, "miy.mc.width", 854).clamp(320, 7680);
    let height = setting_u64(&snap, "miy.mc.height", 480).clamp(240, 4320);
    // PCL2 的「内存分配」，默认 2048 MB。给太小 JVM 起不来，给太大只是白占。
    // 版本自己设过就用它，否则用「核心设置」里的默认值。
    let memory = instance_memory(&version_dir)
        .unwrap_or_else(|| setting_u64(&snap, "miy.mc.memory", 2048))
        .clamp(512, 65536);

    let table: Vec<(&str, String)> = vec![
        ("classpath_separator", ";".to_string()),
        ("library_directory", format!("{root}\\libraries")),
        ("libraries_directory", format!("{root}\\libraries")),
        ("auth_player_name", user.clone()),
        ("version_name", id.to_string()),
        ("version_type", "release".to_string()),
        ("game_directory", version_dir.clone()),
        ("game_assets", format!("{root}\\assets\\virtual\\legacy")),
        ("assets_root", format!("{root}\\assets")),
        ("assets_index_name", assets_id.clone()),
        ("primary_jar", client_jar.clone()),
        ("auth_uuid", uuid.to_string()),
        ("auth_access_token", token.to_string()),
        ("access_token", token.to_string()),
        ("auth_session", token.to_string()),
        ("user_type", user_type.to_string()),
        ("user_properties", "{}".to_string()),
        ("clientid", String::new()),
        ("auth_xuid", String::new()),
        ("natives_directory", natives.clone()),
        ("launcher_name", "MIYLauncher".to_string()),
        ("launcher_version", env!("CARGO_PKG_VERSION").to_string()),
        ("resolution_width", width.to_string()),
        ("resolution_height", height.to_string()),
        ("classpath", classpath.clone()),
    ];

    let main_class = v
        .get("mainClass")
        .and_then(Value::as_str)
        .ok_or("该版本缺少启动入口信息")?;
    let java = java_pick.path.clone();
    let java_ver = java_pick.major;

    // JVM 参数优先用版本 JSON 的 `arguments.jvm`（1.13+ 都有）。
    // 自己硬拼必然漏东西：26.x 需要 `--enable-native-access=ALL-UNNAMED`、
    // `-Dorg.lwjgl.system.SharedLibraryExtractPath`、`-Djna.tmpdir`、`-XX:HeapDumpPath`，
    // 少一个都会在某个平台上以"莫名其妙的内存访问违例"收场。
    let mut args: Vec<String> = match jvm_args_json(&v, &table) {
        Some(list) => list,
        None => vec![
            "-XX:HeapDumpPath=MojangTricksIntelDriversForPerformance_javaw.exe_minecraft.exe.heapdump".to_string(),
            format!("-Djava.library.path={natives}"),
            "-Dminecraft.launcher.brand=MIYLauncher".to_string(),
            format!("-Dminecraft.launcher.version={}", env!("CARGO_PKG_VERSION")),
        ],
    };
    args.push(format!("-Xmx{memory}M"));
    // Xms 不能超过 Xmx，否则 JVM 直接拒绝启动（配 512 MB 内存的用户会踩到）。
    args.push(format!("-Xms{}M", memory.min(512)));
    // GBK 控制台下这三行能让标准流稳定成 UTF-8，日志不会再是乱码。
    args.push("-Dfile.encoding=UTF-8".to_string());
    args.push("-Dstdout.encoding=UTF-8".to_string());
    args.push("-Dstderr.encoding=UTF-8".to_string());
    // 游戏进程自己的临时文件也不许出这个文件夹（见上面 game_tmp 的注释）。
    args.push(format!("-Djava.io.tmpdir={game_tmp}"));
    args.push("-XX:-UsePerfData".to_string());

    // GC 参数照 PCL2 的「内存管理」来；「不指定」就完全不动游戏自带的那些。
    apply_gc_args(&mut args, java_ver, setting_gc(&snap), windows_build());
    // PCL2 会把这两条固定补上：一个拆掉某些整合包塞的保守上限，
    // 一个堵住 log4j 的 JNDI 漏洞。
    args.retain(|a| a != "-XX:MaxDirectMemorySize=256M");
    args.push("-Dlog4j2.formatMsgNoLookups=true".to_string());

    // 自定义 JVM 参数放在最后：去重时保留最后一次出现，玩家写的 `-Xmx` 才盖得掉
    // 启动器算的那个。`-cp` 由我们统一给，玩家的那份丢掉，免得出现两个。
    // 版本自己填了就用版本的，否则用核心设置里的。
    let jvm_text = {
        let own = instance_str(&version_dir, "jvmArgs");
        if own.trim().is_empty() {
            setting_str(&snap, "miy.mc.jvmArgs")
        } else {
            own
        }
    };
    let custom_jvm = arg_replace(&jvm_text, &table);
    args.extend(split_args(&custom_jvm));
    drop_flag_pair(&mut args, "-cp");
    drop_flag_pair(&mut args, "-classpath");
    args = dedupe_keep_last(args);
    args.push("-cp".to_string());
    args.push(classpath.clone());
    args.push(main_class.to_string());

    args.extend(game_args(&v, &table));

    // 自定义游戏参数同样放末尾（PCL2 的 LaunchAdvanceGame），占位符照展开。
    let game_text = {
        let own = instance_str(&version_dir, "gameArgs");
        if own.trim().is_empty() {
            setting_str(&snap, "miy.mc.gameArgs")
        } else {
            own
        }
    };
    let custom_game = arg_replace(&game_text, &table);
    args.extend(split_args(&custom_game));

    // 窗口，对应 PCL2 的 LaunchArgumentWindowType：
    // 0=全屏（options.txt 已写 true，这里补 `--fullscreen`）、1=跟随上次、什么都不动、
    // 3=窗口化（用设置里的宽高）、2=与启动器同尺寸（拿客户区大小，拿不到退回设置值）。
    // 先删掉版本 JSON 或自定义参数里已有的宽高，否则游戏会收到两份。
    drop_flag_pair(&mut args, "--width");
    drop_flag_pair(&mut args, "--height");
    match setting_window(&snap) {
        0 => args.push("--fullscreen".to_string()),
        2 => {
            let (win_w, win_h) = launcher_size(h).unwrap_or((width, height));
            args.push("--width".to_string());
            args.push(win_w.to_string());
            args.push("--height".to_string());
            args.push(win_h.to_string());
        }
        3 => {
            args.push("--width".to_string());
            args.push(width.to_string());
            args.push("--height".to_string());
            args.push(height.to_string());
        }
        _ => {}
    }

    // 只算不跑：给 `--mc-args` 这类诊断用，验证 gameDir / 类路径是否指向这个版本自己的目录。
    if req.get("dry").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(serde_json::to_vec(&json!({
            "ok": true,
            "dry": true,
            "id": id,
            "gameDir": version_dir,
            "root": root,
            "natives": natives,
            "nativesJars": native_jars.len(),
            "assets": assets_id,
            "memory": memory,
            "window": setting_window(&snap),
            "gc": setting_gc(&snap),
            "notes": notes,
            "classpathEntries": cp.len(),
            "classpathHead": cp.iter().take(3).cloned().collect::<Vec<_>>(),
            "args": args.len(),
            // 完整参数表：`--mc-args` 带 MIY_MC_ARGV=1 时逐条打出来，方便核对
            // 自定义参数、GC、窗口这些有没有按 PCL2 的规则拼进去。
            "argv": args,
            "java": java,
            "javaMajor": java_ver,
            "javaSource": java_pick.source,
        }))
        .unwrap_or_default());
    }

    if launch_cancelled() {
        return Ok(launch_cancelled_payload(id, &root));
    }
    let mut cmd = std::process::Command::new(&java);
    cmd.args(&args)
        // 工作目录跟着版本隔离目录走（PCL2 也是 PathIndie）：崩掉之后 JVM 生成的
        // hs_err_pid*.log 落在版本自己的文件夹里，不会散到游戏根目录。
        .current_dir(&version_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd.spawn().map_err(|e| format!("无法启动游戏：{e}"))?;
    let pid = child.id();
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();

    set_running(Some((pid, id.to_string())));
    unsafe { h.emit_str("mc.launch", format!(r#"{{"id":"{id}","pid":{pid},"user":"{user}"}}"#).as_bytes()) };

    // 流式转发日志：游戏可能跑几个小时，绝不能占着派发池线程。
    // stdout 和 stderr 都必须有人读 —— 只读 stdout 时，游戏往 stderr 写满
    // 64 KB 管道就会卡死在 write 上，用户看到的是"点了启动，进程在，窗口没有"。
    let cell = HostCell(*h);
    let id_owned = id.to_string();
    let crash_dir = version_dir.clone();
    // 崩溃分析要拿「实际用的 Java」和「版本要求的 Java」比大小，两个都是 u32，
    // 直接拷进闭包，别让线程去读共享状态。
    let crash_java = java_ver;
    let crash_need = need_java;
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let err_cell = HostCell(cell.0);
        let err_reader = err_pipe.map(|pipe| {
            std::thread::spawn(move || forward_lines(pipe, &err_cell, "[stderr] "))
        });
        if let Some(out) = out_pipe {
            forward_lines(out, &cell, "");
        }
        if let Some(t) = err_reader {
            let _ = t.join();
        }
        let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        set_running(None);
        let ms = started.elapsed().as_millis() as u64;
        let payload = serde_json::to_vec(&json!({"id": id_owned, "code": code, "ms": ms}))
            .unwrap_or_default();
        unsafe { cell.h().emit_str("mc.exit", &payload) };
        // 非正常退出：把「为什么」和日志尾巴一起交给界面，别只留一句"意外退出"。
        if code != 0 {
            let info = crash_info(&crash_dir, code, Some(crash_java), Some(crash_need));
            // 把命中的规则写进日志：用户反馈"它说我是内存不够"时，能一眼看出
            // 是哪条规则判的，而不是去猜。
            unsafe {
                cell.h().log_str(
                    MIY_LOG_INFO,
                    &format!(
                        "崩溃分析：规则 {} → {}",
                        if info.rule.is_empty() { "（未命中）" } else { info.rule },
                        info.reason
                    ),
                );
            }
            let crash = serde_json::to_vec(&json!({
                "id": id_owned,
                "code": code,
                "ms": ms,
                "reason": info.reason,
                "advice": info.advice,
                "rule": info.rule,
                "log": info.log,
                "crash": info.crash,
                "dir": crash_dir,
            }))
            .unwrap_or_default();
            unsafe { cell.h().emit_str("mc.crash", &crash) };
        }
    });

    Ok(serde_json::to_vec(&json!({
        "ok": true,
        "id": id,
        "pid": pid,
        "java": java,
        "javaMajor": java_ver,
        "javaSource": java_pick.source,
        "notes": notes,
        "classpathEntries": cp.len(),
        "args": args.len(),
        "username": user,
        "online": online,
        "root": root,
    }))
    .unwrap_or_default())
}

// ---- ABI 样板 -------------------------------------------------------------

unsafe extern "C" fn start(host: *const MiyHostV1, out_state: *mut *mut c_void) -> i32 {
    if host.is_null() || out_state.is_null() {
        return MIY_ERR_INVALID_ARG;
    }
    let h = &*host;
    if h.abi_version != MIY_ABI_VERSION {
        return MIY_ERR_ABI;
    }
    let st = Box::new(State { host: *h });
    *out_state = Box::into_raw(st).cast();
    h.log_str(MIY_LOG_INFO, "miy.mc: start");
    warm_manifest(h);
    MIY_OK
}

unsafe extern "C" fn stop(state: *mut c_void) {
    if state.is_null() {
        return;
    }
    let st = Box::from_raw(state as *mut State);
    st.host.log_str(MIY_LOG_INFO, "miy.mc: stop");
    drop(st);
}

// ---- 模组（Modrinth）------------------------------------------------------

/// 查询串里的字面量一律百分号编码（facets 是 JSON，[]{},:" 都得转义）。
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 搜索模组：`{query, loader?, mc?, limit?}`
///
/// Modrinth 不需要密钥，只要求带一个能识别的 User-Agent —— 宿主 WinHTTP 的默认
/// UA 就够用，实测 200。
fn service_mods_search(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let modrinth = modrinth_base();
    let raw_q = req.get("query").and_then(Value::as_str).unwrap_or("").trim().to_string();
    // 中文关键词先翻成英文再搜，否则 Modrinth 一个都搜不到。
    let q = if has_cjk(&raw_q) { zh_to_query(&raw_q) } else { raw_q.clone() };
    let q = q.as_str();
    let loader = req.get("loader").and_then(Value::as_str).unwrap_or("");
    let mc = req.get("mc").and_then(Value::as_str).unwrap_or("");
    let limit = req
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .clamp(1, 40);
    // 翻页：Modrinth 的 search 用 offset，页大小由 limit 决定。
    // 它只允许 offset + limit ≤ 10000，越界直接 400，所以这里先夹住。
    let offset = req
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(10_000u64.saturating_sub(limit));

    let mut facets = vec!["[\"project_type:mod\"]".to_string()];
    if !loader.is_empty() {
        facets.push(format!("[\"categories:{loader}\"]"));
    }
    if !mc.is_empty() {
        facets.push(format!("[\"versions:{mc}\"]"));
    }
    let url = format!(
        "{modrinth}/search?query={}&limit={limit}&offset={offset}&index=relevance&facets={}",
        enc(q),
        enc(&format!("[{}]", facets.join(",")))
    );

    let v = fetch_json_race(h, &url)?;
    let hits = v
        .get("hits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let list: Vec<Value> = hits
        .iter()
        .filter_map(|p| {
            Some(json!({
                "id": p.get("project_id").and_then(Value::as_str)?,
                "slug": p.get("slug").and_then(Value::as_str).unwrap_or(""),
                "title": p.get("title").and_then(Value::as_str)?,
                "zh": zh_of(p.get("slug").and_then(Value::as_str).unwrap_or(""))
                    .or_else(|| zh_of(p.get("title").and_then(Value::as_str).unwrap_or("")))
                    .unwrap_or(""),
                "desc": p.get("description").and_then(Value::as_str).unwrap_or(""),
                "downloads": p.get("downloads").and_then(Value::as_u64).unwrap_or(0),
                "icon": p.get("icon_url").and_then(Value::as_str).unwrap_or(""),
            }))
        })
        .collect();

    let out = json!({
        "ok": true,
        "total": v.get("total_hits").and_then(Value::as_u64).unwrap_or(list.len() as u64),
        "offset": offset,
        "hits": list,
    });
    serde_json::to_vec(&out).map_err(|e| e.to_string())
}

/// 安装模组：`{id, mc?, loader?}` → 下载到 `<root>\mods\`
///
/// 版本挑选交给 Modrinth：按 `loaders` / `game_versions` 过滤后它返回的数组已经是
/// 从新到旧排好的，取第一条再用 `primary` 优先挑文件。
fn service_mods_install(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let modrinth = modrinth_base();
    let id = req
        .get("id")
        .and_then(Value::as_str)
        .ok_or("缺少模组 id")?
        .to_string();
    let mc = req.get("mc").and_then(Value::as_str).unwrap_or("");
    let loader = req.get("loader").and_then(Value::as_str).unwrap_or("");
    // 详情页是"这个模组的这一版"，直接把 Modrinth 的版本 id 传进来，不再靠过滤猜。
    let vid = req.get("vid").and_then(Value::as_str).unwrap_or("");

    let first = if !vid.is_empty() {
        fetch_json(h, &format!("{modrinth}/version/{vid}"))?
    } else {
        let mut url = format!("{modrinth}/project/{id}/version");
        let mut qs: Vec<String> = Vec::new();
        if !loader.is_empty() {
            qs.push(format!("loaders={}", enc(&format!("[\"{loader}\"]"))));
        }
        if !mc.is_empty() {
            qs.push(format!("game_versions={}", enc(&format!("[\"{mc}\"]"))));
        }
        if !qs.is_empty() {
            url.push('?');
            url.push_str(&qs.join("&"));
        }
        let v = fetch_json(h, &url)?;
        v.as_array()
            .and_then(|a| a.first())
            .cloned()
            .ok_or("没有和当前游戏版本匹配的文件")?
    };
    let first = &first;
    let files = first
        .get("files")
        .and_then(Value::as_array)
        .ok_or("这个模组没有可下载文件")?;
    let file = files
        .iter()
        .find(|f| f.get("primary").and_then(Value::as_bool).unwrap_or(false))
        .or_else(|| files.first())
        .ok_or("这个模组没有可下载文件")?;
    let furl = file
        .get("url")
        .and_then(Value::as_str)
        .ok_or("下载地址缺失")?;
    let fname = file
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or("mod.jar");

    // 版本已经隔离，模组就必须装进那个版本自己的 mods 目录，否则装完看不见。
    let target = req.get("target").and_then(Value::as_str).unwrap_or("");
    let custom = req.get("dir").and_then(Value::as_str).unwrap_or("");
    let root = game_root(h, req);
    // 目标版本没装加载器的话，模组装进去也不会被加载 —— 直接说清楚，
    // 让前端提示"先装加载器"，别让用户以为装好了。
    if custom.is_empty() && !target.is_empty() {
        let vdir = std::path::Path::new(&root).join("versions");
        let (mc_ver, lname, _, _, _) = describe(&vdir, target);
        if lname == "原版" {
            let need = first
                .get("loaders")
                .and_then(Value::as_array)
                .and_then(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .find(|s| *s != "minecraft")
                })
                .unwrap_or("fabric");
            return Ok(serde_json::to_vec(&json!({
                "ok": false,
                "code": "no_loader",
                "target": target,
                "mc": mc_ver,
                "loader": need,
                "error": format!("「{target}」是原版，没有装 {need}，模组不会生效。"),
            }))
            .unwrap_or_default());
        }
    }
    // 用户自己选了文件夹就装那儿；否则装进目标版本的 mods。
    let dir = if !custom.is_empty() {
        custom.to_string()
    } else if target.is_empty() {
        format!("{root}\\mods")
    } else {
        format!("{root}\\versions\\{target}\\mods")
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("建 mods 目录失败：{e}"))?;
    let path = format!("{dir}\\{fname}");
    let fsize = file.get("size").and_then(Value::as_u64).unwrap_or(0);
    let title = first
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(fname);
    let (task, cancel) = task_new("mod", title, 1);
    let bytes = match download_cancellable(h, &modrinth_urls(furl), &path, fsize, &cancel) {
        Ok(n) => n,
        Err(e) if e == "已取消" => {
            task_cancelled(task, "已取消，本次下载的文件已清理");
            return Ok(serde_json::to_vec(&json!({
                "ok": false,
                "code": "cancelled",
                "task": task,
                "error": "已取消安装。",
            }))
            .unwrap_or_default());
        }
        Err(e) => {
            task_finish(task, false, &e);
            return Err(e);
        }
    };
    task_progress(task, 1, bytes);
    task_finish(task, true, "");

    let out = json!({
        "ok": true,
        "task": task,
        "file": fname,
        "path": path,
        "bytes": bytes,
        "name": first.get("name").and_then(Value::as_str).unwrap_or(""),
        "version": first.get("version_number").and_then(Value::as_str).unwrap_or(""),
    });
    serde_json::to_vec(&out).map_err(|e| e.to_string())
}

/// 某个版本 mods 目录里的 jar 列表。禁用中的文件以 `.disabled` 结尾。
fn service_mod_files(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let root = game_root(h, req);
    let id = field(req, "id").unwrap_or("").trim().to_string();
    if id.is_empty() {
        return Err("缺少版本名".into());
    }
    let dir = format!("{root}\\versions\\{id}\\mods");
    let mut items: Vec<Value> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let lower = name.to_lowercase();
            let (enabled, real) = if lower.ends_with(".jar.disabled") {
                (false, name[..name.len() - 9].to_string())
            } else if lower.ends_with(".jar") {
                (true, name.clone())
            } else {
                continue;
            };
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            items.push(json!({ "file": real, "disabled": !enabled, "size": size }));
        }
    }
    items.sort_by(|a, b| {
        let x = a.get("file").and_then(Value::as_str).unwrap_or("");
        let y = b.get("file").and_then(Value::as_str).unwrap_or("");
        x.to_lowercase().cmp(&y.to_lowercase())
    });
    Ok(serde_json::to_vec(&json!({ "ok": true, "dir": dir, "items": items })).unwrap_or_default())
}

/// 已装模组里能查到「有更新」的那些。
///
/// 判定靠 Modrinth 的 `version_file` 接口：拿本地 jar 的 sha1 反查它属于哪个项目、
/// 哪个版本，再用项目的最新版本对比。这样不需要解析 jar 里的元数据，也不怕改名。
fn service_mod_updates(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let modrinth = modrinth_base();
    let id = field(req, "id").unwrap_or("").trim().to_string();
    if id.is_empty() {
        return Err("缺少版本名".into());
    }
    let mc = req.get("mc").and_then(Value::as_str).unwrap_or("");
    let loader = req.get("loader").and_then(Value::as_str).unwrap_or("");
    let dir = format!("{}\\versions\\{}\\mods", game_root(h, req), id);
    let mut out: Vec<Value> = Vec::new();
    let mut checked = 0u64;
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Ok(json!({ "ok": true, "checked": 0, "updates": [] }).to_string().into_bytes());
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let lower = name.to_lowercase();
        let (enabled, real) = if lower.ends_with(".jar.disabled") {
            (false, name[..name.len() - 9].to_string())
        } else if lower.ends_with(".jar") {
            (true, name.clone())
        } else {
            continue;
        };
        let Ok(bytes) = std::fs::read(e.path()) else { continue };
        let hash = java::sha1_hex(&bytes);
        checked += 1;
        let Ok(cur) = fetch_json(h, &format!("{modrinth}/version_file/{hash}?algorithm=sha1")) else {
            continue;
        };
        let project = cur.get("project_id").and_then(Value::as_str).unwrap_or("");
        let cur_ver = cur.get("id").and_then(Value::as_str).unwrap_or("");
        if project.is_empty() || cur_ver.is_empty() {
            continue;
        }
        let mut url = format!("{modrinth}/project/{project}/version");
        let mut qs: Vec<String> = Vec::new();
        if !loader.is_empty() {
            qs.push(format!("loaders={}", enc(&format!("[\"{loader}\"]"))));
        }
        if !mc.is_empty() {
            qs.push(format!("game_versions={}", enc(&format!("[\"{mc}\"]"))));
        }
        if !qs.is_empty() {
            url.push('?');
            url.push_str(&qs.join("&"));
        }
        let Ok(list) = fetch_json(h, &url) else { continue };
        let Some(arr) = list.as_array() else { continue };
        /* 只对「正式版」提示更新。
           Modrinth 的版本数组按发布时间从新到旧排，第一条常常是 beta/alpha ——
           拿它当"最新版"会天天催用户升级到一个测试版，甚至把稳定版换成测试版。
           所以这里只认 version_type == "release"，并且要求它比本地这个版本更新。 */
        let cur_date = cur.get("date_published").and_then(Value::as_str).unwrap_or("");
        let latest = arr.iter().find(|v| {
            let is_release = v.get("version_type").and_then(Value::as_str) == Some("release");
            if !is_release {
                return false;
            }
            let d = v.get("date_published").and_then(Value::as_str).unwrap_or("");
            // 没有时间戳就退回"id 不同即视为更新"，但绝不拿旧版本当新版本。
            if cur_date.is_empty() || d.is_empty() {
                return true;
            }
            d > cur_date
        });
        let Some(latest) = latest else { continue };
        let latest_id = latest.get("id").and_then(Value::as_str).unwrap_or("");
        if latest_id.is_empty() || latest_id == cur_ver {
            continue;
        }
        let title = latest
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| cur.get("name").and_then(Value::as_str))
            .unwrap_or(&real)
            .to_string();
        out.push(json!({
            "file": real,
            "disabled": !enabled,
            "project": project,
            "title": title,
            "current": cur.get("version_number").and_then(Value::as_str).unwrap_or(""),
            "latest": latest.get("version_number").and_then(Value::as_str).unwrap_or(""),
            "vid": latest_id,
        }));
    }
    Ok(json!({ "ok": true, "checked": checked, "updates": out })
        .to_string()
        .into_bytes())
}

/// 把某个模组更新到指定版本：先下到 `.part`，成功后再替换旧文件。
fn service_mod_update(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let modrinth = modrinth_base();
    let id = field(req, "id").unwrap_or("").trim().to_string();
    let file = req.get("file").and_then(Value::as_str).unwrap_or("").to_string();
    let vid = req.get("vid").and_then(Value::as_str).unwrap_or("").to_string();
    if id.is_empty() || file.is_empty() || vid.is_empty() {
        return Err("缺少要更新的文件".into());
    }
    if file.contains('\\') || file.contains('/') || file.contains("..") {
        return Err("文件名不合法".into());
    }
    let dir = format!("{}\\versions\\{}\\mods", game_root(h, req), id);
    let v = fetch_json(h, &format!("{modrinth}/version/{vid}"))?;
    let files = v
        .get("files")
        .and_then(Value::as_array)
        .ok_or("这个版本没有可下载文件")?;
    let f = files
        .iter()
        .find(|f| f.get("primary").and_then(Value::as_bool).unwrap_or(false))
        .or_else(|| files.first())
        .ok_or("这个版本没有可下载文件")?;
    let url = f.get("url").and_then(Value::as_str).ok_or("下载地址缺失")?;
    let name = f
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or("mod.jar")
        .to_string();
    let tmp = format!("{dir}\\{name}.part");
    download(h, url, &tmp, 0)?;
    let target = format!("{dir}\\{name}");
    if std::path::Path::new(&target).exists() {
        let _ = std::fs::remove_file(&target);
    }
    std::fs::rename(&tmp, &target).map_err(|e| format!("替换文件失败：{e}"))?;
    if name != file {
        let _ = std::fs::remove_file(format!("{dir}\\{file}"));
    }
    Ok(json!({ "ok": true, "file": name }).to_string().into_bytes())
}

/// 启用/禁用：只改文件名的 `.disabled` 后缀，不删文件。
fn service_mod_toggle(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let root = game_root(h, req);
    let id = field(req, "id").unwrap_or("").trim().to_string();
    let file = field(req, "file").unwrap_or("").trim().to_string();
    if id.is_empty() || file.is_empty() {
        return Err("缺少版本名或文件名".into());
    }
    if file.contains('\\') || file.contains('/') || file.contains("..") {
        return Err("文件名不合法".into());
    }
    let enable = req.get("enable").and_then(Value::as_bool).unwrap_or(true);
    let dir = format!("{root}\\versions\\{id}\\mods");
    let on = format!("{dir}\\{file}");
    let off = format!("{on}.disabled");
    if enable {
        if !std::path::Path::new(&off).exists() {
            return Ok(serde_json::to_vec(&json!({ "ok": true, "disabled": false })).unwrap_or_default());
        }
        std::fs::rename(&off, &on).map_err(|e| format!("无法启用 {file}：{e}"))?;
    } else {
        if !std::path::Path::new(&on).exists() {
            return Ok(serde_json::to_vec(&json!({ "ok": true, "disabled": true })).unwrap_or_default());
        }
        std::fs::rename(&on, &off).map_err(|e| format!("无法禁用 {file}：{e}"))?;
    }
    Ok(serde_json::to_vec(&json!({ "ok": true, "disabled": !enable })).unwrap_or_default())
}

// ---- CurseForge（走 MCIM 镜像，不需要 API Key）-----------------------------

const CF_GAME_MC: u64 = 432;
const CF_CLASS_MODS: u64 = 6;

fn cf_loader(loader: &str) -> u64 {
    match loader {
        "forge" => 1,
        "fabric" => 4,
        "quilt" => 5,
        "neoforge" => 6,
        _ => 0,
    }
}

fn cf_get_once(h: &MiyHostV1, url: &str) -> Result<Value, (String, bool)> {
    let payload = json!({
        "url": url,
        "timeout_ms": 30000,
        "max_bytes": 8 * 1024 * 1024,
        "headers": { "Accept": "application/json" },
    });
    let body = serde_json::to_vec(&payload).unwrap_or_default();
    let reply = unsafe { h.call_json("miy.net.get", &body) }
        .ok_or(("网络请求失败".to_string(), true))?;
    let v: Value = serde_json::from_slice(&reply).map_err(|e| (e.to_string(), true))?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        let status = v.get("status").and_then(Value::as_u64).unwrap_or(0);
        let err = v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("请求失败")
            .to_string();
        let site_problem = status == 0 || status == 429 || status >= 500;
        return Err((err, site_problem));
    }
    let text = v.get("text").and_then(Value::as_str).unwrap_or("");
    serde_json::from_str(text).map_err(|e| (format!("返回内容不是 JSON：{e}"), false))
}

/// 正在竞速的 CurseForge 请求数：MCIM 的搜索要 8–17 秒，输掉的那个线程还得
/// 跑完才退出。用户连续翻页时别无限堆线程，超过上限就只走自建中转。
static CF_RACING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const CF_RACE_MAX: usize = 4;

/// CurseForge 的请求：MCIM 与自建中转**并行竞速**，谁先回用谁。
///
/// 实测 MCIM 的搜索接口 8–17 秒（详情/文件 0.15–0.3 秒），自建中转裁包后 1.2 秒。
/// 两个都发、先到的赢，慢的那个结果直接丢掉；都失败才回官方源。
fn cf_get(h: &MiyHostV1, url: &str) -> Result<Value, String> {
    use std::sync::atomic::Ordering;
    let official = direct_of(url);
    let mut cands: Vec<String> = candidates(url)
        .into_iter()
        .filter(|c| c != &official)
        .collect();
    // 竞速只在「两个候选都在」且没超过并发上限时启用。
    let racing = cands.len() >= 2 && CF_RACING.load(Ordering::Relaxed) < CF_RACE_MAX;
    if racing {
        CF_RACING.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel::<(String, Result<Value, (String, bool)>)>();
        for c in cands.drain(..) {
            let tx = tx.clone();
            let cell = HostCell(*h);
            std::thread::spawn(move || {
                let r = cf_get_once(cell.h(), &c);
                let _ = tx.send((c, r));
            });
        }
        drop(tx);
        let mut last = String::from("没有可用的请求源");
        for (c, r) in rx {
            match r {
                Ok(v) => {
                    CF_RACING.fetch_sub(1, Ordering::Relaxed);
                    if is_site(&c) {
                        site_recover();
                    }
                    return Ok(v);
                }
                Err((e, site_problem)) => {
                    if is_site(&c) && site_problem {
                        site_fail();
                        site_dead_mark();
                    }
                    last = e;
                }
            }
        }
        CF_RACING.fetch_sub(1, Ordering::Relaxed);
        if official != url {
            return cf_get_once(h, &official)
                .map_err(|(e, _)| format!("{last}；官方源也失败：{e}"));
        }
        return Err(last);
    }
    let mut last = String::from("没有可用的请求源");
    for c in candidates(url) {
        match cf_get_once(h, &c) {
            Ok(v) => {
                if is_site(&c) {
                    site_recover();
                }
                return Ok(v);
            }
            Err((e, site_problem)) => {
                if is_site(&c) && site_problem {
                    site_fail();
                    site_dead_mark();
                }
                last = e;
            }
        }
    }
    Err(last)
}

/// `{query, mc, loader, page}` → 与 Modrinth 搜索同形的 `{hits, total}`。
fn service_cf_search(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let curseforge = curseforge_base();
    let q = req.get("query").and_then(Value::as_str).unwrap_or("").trim();
    let mc = req.get("mc").and_then(Value::as_str).unwrap_or("");
    let loader = req.get("loader").and_then(Value::as_str).unwrap_or("");
    let page = req.get("page").and_then(Value::as_u64).unwrap_or(1).max(1);
    let size = req.get("limit").and_then(Value::as_u64).unwrap_or(20).clamp(1, 50);

    let mut url = format!(
        "{curseforge}/mods/search?gameId={CF_GAME_MC}&classId={CF_CLASS_MODS}&sortField=2&sortOrder=desc&index={}&pageSize={size}",
        // CurseForge 同样只允许 index ≤ 10000，越界会返回空数组。
        ((page - 1) * size).min(10_000)
    );
    if !q.is_empty() {
        url.push_str(&format!("&searchFilter={}", enc(q)));
    }
    if !mc.is_empty() {
        url.push_str(&format!("&gameVersion={}", enc(mc)));
    }
    let lt = cf_loader(loader);
    if lt > 0 {
        url.push_str(&format!("&modLoaderType={lt}"));
    }

    let v = cf_get(h, &url)?;
    let data = v.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
    let total = v
        .get("pagination")
        .and_then(|p| p.get("totalCount"))
        .and_then(Value::as_u64)
        .unwrap_or(data.len() as u64);

    let hits: Vec<Value> = data
        .iter()
        .map(|m| {
            let id = m.get("id").and_then(Value::as_u64).unwrap_or(0);
            let logo = m
                .get("logo")
                .and_then(|l| l.get("thumbnailUrl"))
                .and_then(Value::as_str)
                .unwrap_or("");
            json!({
                "id": format!("cf:{id}"),
                "title": m.get("name").and_then(Value::as_str).unwrap_or(""),
                "description": m.get("summary").and_then(Value::as_str).unwrap_or(""),
                "desc": m.get("summary").and_then(Value::as_str).unwrap_or(""),
                "downloads": m.get("downloadCount").and_then(Value::as_u64).unwrap_or(0),
                "icon": logo,
                "source": "curseforge",
            })
        })
        .collect();

    Ok(serde_json::to_vec(&json!({ "ok": true, "hits": hits, "total": total })).unwrap_or_default())
}

/// `{id:"cf:123", mc, loader, target|dir}` → 装最新匹配文件。
fn service_cf_install(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let curseforge = curseforge_base();
    let raw = req.get("id").and_then(Value::as_str).unwrap_or("");
    let id = raw.trim_start_matches("cf:");
    if id.is_empty() {
        return Err("缺少模组 id".into());
    }
    let mc = req.get("mc").and_then(Value::as_str).unwrap_or("");
    let loader = req.get("loader").and_then(Value::as_str).unwrap_or("");

    let mut url = format!("{curseforge}/mods/{id}/files?pageSize=20");
    if !mc.is_empty() {
        url.push_str(&format!("&gameVersion={}", enc(mc)));
    }
    let lt = cf_loader(loader);
    if lt > 0 {
        url.push_str(&format!("&modLoaderType={lt}"));
    }
    let v = cf_get(h, &url)?;
    let files = v.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
    let file = files
        .iter()
        .find(|f| f.get("downloadUrl").and_then(Value::as_str).is_some())
        .ok_or("这个模组没有可直接下载的文件（作者可能关闭了第三方下载）")?;
    let furl = file.get("downloadUrl").and_then(Value::as_str).unwrap_or("");
    let fname = file.get("fileName").and_then(Value::as_str).unwrap_or("mod.jar");

    let target = req.get("target").and_then(Value::as_str).unwrap_or("");
    let custom = req.get("dir").and_then(Value::as_str).unwrap_or("");
    let root = game_root(h, req);
    let dir = if !custom.is_empty() {
        custom.to_string()
    } else if target.is_empty() {
        format!("{root}\\mods")
    } else {
        format!("{root}\\versions\\{target}\\mods")
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("建 mods 目录失败：{e}"))?;
    let path = format!("{dir}\\{fname}");
    let fsize = file.get("fileLength").and_then(Value::as_u64).unwrap_or(0);
    let title = file
        .get("displayName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(fname);
    let (task, cancel) = task_new("mod", title, 1);
    let bytes = match download_cancellable(h, &modrinth_urls(furl), &path, fsize, &cancel) {
        Ok(n) => n,
        Err(e) if e == "已取消" => {
            task_cancelled(task, "已取消，本次下载的文件已清理");
            return Ok(serde_json::to_vec(&json!({
                "ok": false,
                "code": "cancelled",
                "task": task,
                "error": "已取消安装。",
            }))
            .unwrap_or_default());
        }
        Err(e) => {
            task_finish(task, false, &e);
            return Err(e);
        }
    };
    task_progress(task, 1, bytes);
    task_finish(task, true, "");
    Ok(serde_json::to_vec(&json!({
        "ok": true,
        "task": task,
        "file": fname,
        "path": path,
        "bytes": bytes,
        "source": "curseforge",
    }))
    .unwrap_or_default())
}

/// CurseForge 模组详情：`{id:"cf:123", mc, loader}` → 项目信息 + 可用文件。
fn service_cf_mod(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let curseforge = curseforge_base();
    let raw = req.get("id").and_then(Value::as_str).unwrap_or("");
    let id = raw.trim_start_matches("cf:");
    if id.is_empty() {
        return Err("缺少模组 id".into());
    }
    let loader = req.get("loader").and_then(Value::as_str).unwrap_or("");

    let p = cf_get(h, &format!("{curseforge}/mods/{id}"))?;
    let m = p.get("data").cloned().unwrap_or(Value::Null);
    let logo = m
        .get("logo")
        .and_then(|l| l.get("thumbnailUrl"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let cats: Vec<String> = m
        .get("categories")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|c| c.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    // 不按 gameVersion 过滤：详情页要把「适用于当前版本」和「其他版本」分成两栏，
    // 服务端一过滤就永远拿不到后者。取最近 100 个文件，交给界面分组。
    let mut url = format!("{curseforge}/mods/{id}/files?pageSize=100");
    let lt = cf_loader(loader);
    if lt > 0 {
        url.push_str(&format!("&modLoaderType={lt}"));
    }
    let fv = cf_get(h, &url)?;
    let files: Vec<Value> = fv
        .get("data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|f| f.get("downloadUrl").and_then(Value::as_str).is_some())
                .map(|f| {
                    json!({
                        "vid": f.get("id").and_then(Value::as_u64).unwrap_or(0).to_string(),
                        "name": f.get("displayName").and_then(Value::as_str).unwrap_or(""),
                        "version": f.get("displayName").and_then(Value::as_str).unwrap_or(""),
                        "file": f.get("fileName").and_then(Value::as_str).unwrap_or(""),
                        "downloads": f.get("downloadCount").and_then(Value::as_u64).unwrap_or(0),
                        "size": f.get("fileLength").and_then(Value::as_u64).unwrap_or(0),
                        "date": f.get("fileDate").and_then(Value::as_str).unwrap_or(""),
                        // CurseForge 把 MC 版本和加载器混在 gameVersions 里，
                        // 界面只用来判断「是否适用于当前版本」。
                        "game_versions": f.get("gameVersions").and_then(Value::as_array).cloned().unwrap_or_default(),
                        "loaders": [],
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(serde_json::to_vec(&json!({
        "ok": true,
        "source": "curseforge",
        "title": m.get("name").and_then(Value::as_str).unwrap_or(""),
        "desc": m.get("summary").and_then(Value::as_str).unwrap_or(""),
        "icon": logo,
        "downloads": m.get("downloadCount").and_then(Value::as_u64).unwrap_or(0),
        "categories": cats,
        "loaders": [],
        "files": files,
    }))
    .unwrap_or_default())
}

// ---- 模组中文名 -----------------------------------------------------------
//
// 只收「中文玩家真的会这么叫」的常见模组：英文 slug / 标题 → 中文名。
// 中文搜索靠这张表反查成英文关键词，再交给 Modrinth 搜。

// 中文名表放在 data/zh_mods.tsv（slug<TAB>中文名），编译期嵌进二进制。
// 单独成文件是为了能持续往里加，不用每次改代码。
const ZH_TSV: &str = include_str!("../data/zh_mods.tsv");

fn zh_table() -> &'static Vec<(&'static str, &'static str)> {
    static TABLE: std::sync::OnceLock<Vec<(&'static str, &'static str)>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut out = Vec::new();
        for line in ZH_TSV.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.splitn(2, '\t');
            let (Some(slug), Some(name)) = (it.next(), it.next()) else {
                continue;
            };
            let slug = slug.trim();
            let name = name.trim();
            if !slug.is_empty() && !name.is_empty() {
                out.push((slug, name));
            }
        }
        out
    })
}

fn zh_of(text: &str) -> Option<&'static str> {
    let t = text.to_lowercase();
    let t = t.trim();
    if t.is_empty() {
        return None;
    }
    // 只认「完全相同」：slug 或标题整串等于表里的键。
    //
    // 以前还做「最长子串匹配」，结果 `reeses-sodium-options` 里含 `sodium`，
    // 被标成「钠」——名字对不上比没有中文名更糟，所以宁可不标。
    // 需要中文名的模组，往 data/zh_mods.tsv 里加一行它的完整 slug 即可。
    for (k, v) in zh_table() {
        if t == *k {
            return Some(v);
        }
    }
    None
}

/// 中文关键词 → 英文搜索词。命中不了就原样返回。
fn zh_to_query(q: &str) -> String {
    let q = q.trim();
    if q.is_empty() {
        return String::new();
    }
    for (en, zh) in zh_table() {
        if q == *zh || zh.contains(q) {
            return en.replace('-', " ");
        }
    }
    q.to_string()
}

fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

/// 模组详情：`{id}` → 项目信息（详情页头部用）。
fn service_mod_detail(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let modrinth = modrinth_base();
    let id = req
        .get("id")
        .and_then(Value::as_str)
        .ok_or("缺少模组 id")?;
    let p = fetch_json(h, &format!("{modrinth}/project/{id}"))?;
    let arr = |k: &str| -> Vec<String> {
        p.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let out = json!({
        "ok": true,
        "id": p.get("id").and_then(Value::as_str).unwrap_or(id),
        "slug": p.get("slug").and_then(Value::as_str).unwrap_or(""),
        "title": p.get("title").and_then(Value::as_str).unwrap_or(""),
        "desc": p.get("description").and_then(Value::as_str).unwrap_or(""),
        "downloads": p.get("downloads").and_then(Value::as_u64).unwrap_or(0),
        "follows": p.get("followers").and_then(Value::as_u64).unwrap_or(0),
        "icon": p.get("icon_url").and_then(Value::as_str).unwrap_or(""),
        "categories": arr("categories"),
        "loaders": arr("loaders"),
        "game_versions": arr("game_versions"),
        "updated": p.get("updated").and_then(Value::as_str).unwrap_or(""),
        "source": p.get("source_url").and_then(Value::as_str).unwrap_or(""),
    });
    serde_json::to_vec(&out).map_err(|e| e.to_string())
}

/// 模组的所有版本：`{id, mc?, loader?, limit?}`。
///
/// 详情页靠它列"模组版本 × 对应 MC 版本 + 加载器"，所以每条都带上
/// `game_versions` / `loaders`，让界面能分组而不是只显示一个版本号。
/// 正式版 id 全集（`type == "release"`），进程内只取一次清单。
fn release_set(h: &MiyHostV1) -> Vec<String> {
    static CACHE: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let Ok(man) = manifest(h) else {
                return Vec::new();
            };
            man.get("versions")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter(|v| v.get("type").and_then(Value::as_str) == Some("release"))
                        .filter_map(|v| v.get("id").and_then(Value::as_str).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        })
        .clone()
}

fn service_mod_versions(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let modrinth = modrinth_base();
    let id = req
        .get("id")
        .and_then(Value::as_str)
        .ok_or("缺少模组 id")?;
    let mc = req.get("mc").and_then(Value::as_str).unwrap_or("");
    let loader = req.get("loader").and_then(Value::as_str).unwrap_or("");
    // 详情页来回切模组时会反复问同一份清单：内存里留 5 分钟，切回来是 0 毫秒。
    let key = format!("{id}|{mc}|{loader}");
    if let Ok(g) = MODVER_CACHE.lock() {
        if let Some((_, _, body)) = g
            .iter()
            .find(|(k, at, _)| k == &key && at.elapsed().as_secs() < 300)
        {
            return Ok(body.clone());
        }
    }
    // 详情页要能翻到历史旧版本：Modrinth 的 `/project/{id}/version` 不分页，
    // 一次给全（大模组几百条），所以默认就把上限开大，别只取最新 40 条。
    let limit = req
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(300)
        .clamp(1, 500) as usize;

    let mut url = format!("{modrinth}/project/{id}/version");
    let mut qs: Vec<String> = Vec::new();
    if !loader.is_empty() {
        qs.push(format!("loaders={}", enc(&format!("[\"{loader}\"]"))));
    }
    if !mc.is_empty() {
        qs.push(format!("game_versions={}", enc(&format!("[\"{mc}\"]"))));
    }
    if !qs.is_empty() {
        url.push('?');
        url.push_str(&qs.join("&"));
    }

    // 版本列表动辄几百条、几百 KB：MCIM 传大包要十几秒，自建中转有缓存（热命中几十毫秒），
    // 官方源居中。并行发、谁先回用谁，别让最慢的那个拖住整个详情页。
    let mut v = fetch_json_race(h, &url)?;
    // 自建中转把版本对象裁成了精简版（没有 dependencies），前置模组就全丢了。
    // 整份都没有这个字段时回官方源再要一次 —— 慢一点，但前置信息是对的。
    let lossy = v
        .as_array()
        .map(|a| !a.is_empty() && a.iter().all(|x| x.get("dependencies").is_none()))
        .unwrap_or(false);
    if lossy {
        let official = direct_of(&url);
        if official != url {
            if let Ok(v2) = fetch_json_once(h, &official) {
                v = v2;
            }
        }
    }
    let mut list: Vec<Value> = v
        .as_array()
        .map(|a| {
            a.iter()
                .take(limit)
                .filter_map(|ver| {
                    let files = ver.get("files").and_then(Value::as_array)?;
                    let file = files
                        .iter()
                        .find(|f| f.get("primary").and_then(Value::as_bool).unwrap_or(false))
                        .or_else(|| files.first())?;
                    let strs = |k: &str| -> Vec<String> {
                        ver.get(k)
                            .and_then(Value::as_array)
                            .map(|x| {
                                x.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default()
                    };
                    // 前置模组：只留 required（optional / incompatible / embedded 不提示）。
                    // 详情页据此在安装前弹一张「需要前置」的清单。
                    let deps: Vec<Value> = ver
                        .get("dependencies")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter(|d| {
                                    d.get("dependency_type").and_then(Value::as_str)
                                        == Some("required")
                                })
                                .filter_map(|d| {
                                    let project =
                                        d.get("project_id").and_then(Value::as_str).unwrap_or("");
                                    let version =
                                        d.get("version_id").and_then(Value::as_str).unwrap_or("");
                                    if project.is_empty() && version.is_empty() {
                                        return None;
                                    }
                                    Some(json!({ "project": project, "version": version }))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(json!({
                        "id": ver.get("id").and_then(Value::as_str)?,
                        "name": ver.get("name").and_then(Value::as_str).unwrap_or(""),
                        "number": ver.get("version_number").and_then(Value::as_str).unwrap_or(""),
                        "game_versions": strs("game_versions"),
                        "loaders": strs("loaders"),
                        "date": ver.get("date_published").and_then(Value::as_str).unwrap_or(""),
                        "downloads": ver.get("downloads").and_then(Value::as_u64).unwrap_or(0),
                        "file": file.get("filename").and_then(Value::as_str).unwrap_or(""),
                        "size": file.get("size").and_then(Value::as_u64).unwrap_or(0),
                        "deps": deps,
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    // 只留正式版：Modrinth 的 `game_versions` 把快照版也列进来，而挂在快照版上的
    // 构件基本只有作者自己试过（PCL2 同样默认只显示正式版）。
    // 清单拿不到时（离线）就不过滤，宁可多列也不要空列表。
    let releases = release_set(h);
    if !releases.is_empty() {
        list.retain_mut(|v| {
            let Some(arr) = v.get("game_versions").and_then(Value::as_array).cloned() else {
                return false;
            };
            let keep: Vec<Value> = arr
                .into_iter()
                .filter(|x| {
                    x.as_str()
                        .map(|s| releases.iter().any(|r| r == s))
                        .unwrap_or(false)
                })
                .collect();
            if keep.is_empty() {
                return false;
            }
            if let Some(o) = v.as_object_mut() {
                o.insert("game_versions".to_string(), Value::Array(keep));
            }
            true
        });
    }

    // 详情页那两个下拉：这个模组到底支持哪些 MC 版本、支持哪些加载器。
    // 从全量版本里汇总，而不是让前端自己去猜。
    let mut mcs: Vec<String> = list
        .iter()
        .flat_map(|v| {
            v.get("game_versions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|x| x.as_str().map(str::to_string))
        .collect();
    mcs.sort_by(|a, b| ver_cmp(b, a));
    mcs.dedup();
    let mut loaders: Vec<String> = list
        .iter()
        .flat_map(|v| {
            v.get("loaders")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|x| x.as_str().map(str::to_string))
        .collect();
    loaders.sort();
    loaders.dedup();

    let out = json!({
        "ok": true,
        "count": list.len(),
        "mc_versions": mcs,
        "loaders": loaders,
        "versions": list,
    });
    let body = serde_json::to_vec(&out).map_err(|e| e.to_string())?;
    if let Ok(mut g) = MODVER_CACHE.lock() {
        g.retain(|(k, at, _)| k != &key && at.elapsed().as_secs() < 300);
        g.push((key, std::time::Instant::now(), body.clone()));
        if g.len() > 24 {
            g.remove(0);
        }
    }
    Ok(body)
}

/// 前置模组的详情：给一批 Modrinth 版本 id，返回它们属于哪个项目、叫什么、装哪个文件。
///
/// 详情页点「安装」时如果这一版有 required 前置，就先问这里，弹一张清单让用户
/// 一起装或跳过去看 —— 和 PCL2 的「缺少前置」提示一个意思。
/// 前置模组里没钉版本号的那种（Modrinth 允许只依赖项目），按当前 MC 版本和加载器
/// 挑一个能用的：优先正式版，其次最新的那个。
fn pick_project_version(h: &MiyHostV1, base: &str, pid: &str, mc: &str, loader: &str) -> Option<Value> {
    let v = fetch_json_race(h, &format!("{base}/project/{pid}/version")).ok()?;
    let arr = v.as_array()?;
    let fits = |x: &Value| -> bool {
        let g = x
            .get("game_versions")
            .and_then(Value::as_array)
            .map(|a| mc.is_empty() || a.iter().any(|s| s.as_str() == Some(mc)))
            .unwrap_or(true);
        let l = x
            .get("loaders")
            .and_then(Value::as_array)
            .map(|a| loader.is_empty() || a.iter().any(|s| s.as_str() == Some(loader)))
            .unwrap_or(true);
        g && l
    };
    let cand: Vec<&Value> = arr.iter().filter(|x| fits(x)).collect();
    let pick = cand
        .iter()
        .find(|x| x.get("version_type").and_then(Value::as_str) == Some("release"))
        .or_else(|| cand.first())?;
    let mut v = (*pick).clone();
    // 自建中转的精简版本对象里没有 project_id，补上——后面要靠它查标题和图标。
    if v.get("project_id").and_then(Value::as_str).unwrap_or("").is_empty() {
        if let Some(o) = v.as_object_mut() {
            o.insert("project_id".to_string(), json!(pid));
        }
    }
    Some(v)
}

fn service_deps(h: &MiyHostV1, req: &Value) -> Result<Vec<u8>, String> {
    let modrinth = modrinth_base();
    let mc = req.get("mc").and_then(Value::as_str).unwrap_or("");
    let loader = req.get("loader").and_then(Value::as_str).unwrap_or("");
    let ids: Vec<String> = req
        .get("versions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let loose: Vec<String> = req
        .get("projects")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() && loose.is_empty() {
        return Ok(serde_json::to_vec(&json!({ "ok": true, "deps": [] })).unwrap_or_default());
    }
    let mut arr: Vec<Value> = Vec::new();
    if !ids.is_empty() {
        let ids_json = serde_json::to_string(&ids).unwrap_or_else(|_| "[]".into());
        let vers = fetch_json_race(h, &format!("{modrinth}/versions?ids={}", enc(&ids_json)))
            .unwrap_or(Value::Array(Vec::new()));
        arr.extend(vers.as_array().cloned().unwrap_or_default());
    }
    // 只依赖项目、没钉版本号的前置：按当前 MC + 加载器挑一个。
    for pid in &loose {
        if let Some(v) = pick_project_version(h, &modrinth, pid, mc, loader) {
            arr.push(v);
        }
    }

    // 项目信息（标题 / 图标 / slug）一次批量取，别一个前置发一次请求。
    let mut pids: Vec<String> = arr
        .iter()
        .filter_map(|v| {
            v.get("project_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    pids.sort();
    pids.dedup();
    let mut projects: HashMap<String, Value> = HashMap::new();
    if !pids.is_empty() {
        let pj = serde_json::to_string(&pids).unwrap_or_else(|_| "[]".into());
        if let Ok(v) = fetch_json_race(h, &format!("{modrinth}/projects?ids={}", enc(&pj))) {
            for p in v.as_array().cloned().unwrap_or_default() {
                if let Some(id) = p.get("id").and_then(Value::as_str) {
                    projects.insert(id.to_string(), p);
                }
            }
        }
    }

    let deps: Vec<Value> = arr
        .iter()
        .map(|v| {
            let pid = v.get("project_id").and_then(Value::as_str).unwrap_or("");
            let p = projects.get(pid);
            let file = v.get("files").and_then(Value::as_array).and_then(|f| {
                f.iter()
                    .find(|x| x.get("primary").and_then(Value::as_bool).unwrap_or(false))
                    .or_else(|| f.first())
            });
            let s = |k: &str| p.and_then(|p| p.get(k)).and_then(Value::as_str).unwrap_or("");
            json!({
                "version": v.get("id").and_then(Value::as_str).unwrap_or(""),
                "project": pid,
                "number": v.get("version_number").and_then(Value::as_str).unwrap_or(""),
                "title": if s("title").is_empty() { pid } else { s("title") },
                "slug": s("slug"),
                "icon": s("icon_url"),
                "desc": s("description"),
                "file": file.and_then(|f| f.get("filename")).and_then(Value::as_str).unwrap_or(""),
                "size": file.and_then(|f| f.get("size")).and_then(Value::as_u64).unwrap_or(0),
            })
        })
        .collect();
    Ok(serde_json::to_vec(&json!({ "ok": true, "deps": deps })).unwrap_or_default())
}

/// 版本号比较：`26.3` 要排在 `1.21.5` 前面，不能按字符串比。
fn ver_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn key(s: &str) -> Vec<i64> {
        s.split(|c: char| !c.is_ascii_digit())
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.parse::<i64>().ok())
            .collect()
    }
    key(a).cmp(&key(b)).then_with(|| a.cmp(b))
}

unsafe fn dispatch_inner(
    state: *mut c_void,
    service: *const u8,
    service_len: usize,
    req: *const u8,
    req_len: usize,
    out: *mut *mut u8,
) -> i32 {
    if state.is_null() || out.is_null() || (service.is_null() && service_len != 0) {
        return MIY_ERR_INVALID_ARG;
    }
    let st = &*(state as *const State);
    let name = core::str::from_utf8_unchecked(core::slice::from_raw_parts(service, service_len));
    let body = if req.is_null() || req_len == 0 {
        &[][..]
    } else {
        core::slice::from_raw_parts(req, req_len)
    };
    let req = body_json(body);

    let result = match name {
        "miy.mc.versions" => service_versions(&st.host),
        "miy.mc.manifest.refresh" => service_manifest_refresh(&st.host),
        "miy.mc.installed" => service_installed(&st.host, &req),
        "miy.mc.state.get" => service_state_get(&st.host, &req),
        "miy.mc.state.set" => service_state_set(&st.host, &req),
        "miy.mc.roots" => service_roots(&st.host, &req),
        "miy.mc.root.add" => service_root_add(&st.host, &req),
        "miy.mc.root.remove" => service_root_remove(&st.host, &req),
        "miy.mc.root.rename" => service_root_rename(&st.host, &req),
        "miy.mc.import" => service_import(&st.host, &req),
        "miy.mc.modpack.install" => service_modpack_install(&st.host, &req),
        "miy.mc.remove" => service_remove(&st.host, &req),
        "miy.mc.rename" => service_rename(&st.host, &req),
        "miy.mc.info" => service_info(&st.host, &req),
        "miy.mc.verinfo" => service_verinfo(&st.host, &req),
        "miy.mc.install" => service_install(&st.host, &req),
        "miy.mc.loaders" => service_loaders(&st.host, &req),
        "miy.mc.loader.install" => service_loader_install(&st.host, &req),
        "miy.mc.assets" => service_assets(&st.host, &req),
        "miy.mc.repair" => service_repair(&st.host, &req),
        "miy.mc.memory" => service_memory(&st.host, &req),
        "miy.mc.vsettings" => service_vsettings(&st.host, &req),
        "miy.mc.folder" => service_folder(&st.host, &req),
        "miy.mc.launch" => service_launch(&st.host, &req),
        "miy.mc.running" => service_running(&st.host, &req),
        "miy.mc.modicons" => service_modicons(&st.host, &req),
        "miy.mc.export" => service_export(&st.host, &req),
        "miy.mc.export.list" => service_export_list(&st.host, &req),
        "miy.mc.modremove" => service_modremove(&st.host, &req),
        "miy.mc.kill" => service_kill(&st.host, &req),
        "miy.mc.launch.cancel" => service_launch_cancel(&st.host, &req),
        "miy.mc.javas" => service_javas(&st.host, &req),
        "miy.mc.java.install" => service_java_install(&st.host, &req),
        "miy.mc.mods" => service_mods_search(&st.host, &req),
        "miy.mc.tasks" => Ok(tasks_json().into_bytes()),
        "miy.mc.task.cancel" => {
            let id = req.get("id").and_then(Value::as_u64).unwrap_or(0);
            let ok = id != 0 && task_cancel(id);
            st.host.log_str(
                MIY_LOG_INFO,
                &format!("miy.mc: task.cancel id={id} ok={ok}"),
            );
            Ok(json!({ "ok": ok, "id": id }).to_string().into_bytes())
        }
        "miy.mc.mod" => service_mod_detail(&st.host, &req),
        "miy.mc.mod.versions" => service_mod_versions(&st.host, &req),
        "miy.mc.deps" => service_deps(&st.host, &req),
        "miy.mc.mod.install" => service_mods_install(&st.host, &req),
        "miy.mc.modfiles" => service_mod_files(&st.host, &req),
        "miy.mc.modtoggle" => service_mod_toggle(&st.host, &req),
            "miy.mc.modupdates" => service_mod_updates(&st.host, &req),
            "miy.mc.modupdate" => service_mod_update(&st.host, &req),
        "miy.mc.cf.search" => service_cf_search(&st.host, &req),
        "miy.mc.cf.install" => service_cf_install(&st.host, &req),
        "miy.mc.cf.mod" => service_cf_mod(&st.host, &req),
        _ => return MIY_ERR_UNKNOWN_SERVICE,
    };

    let payload = match result {
        Ok(v) => v,
        Err(e) => {
            let msg = serde_json::to_vec(&json!({"ok": false, "error": e})).unwrap_or_default();
            let buf = st.host.alloc_buf(&msg);
            if buf.is_null() {
                return MIY_ERR_INVALID_ARG;
            }
            *out = buf;
            return MIY_OK;
        }
    };

    let buf = st.host.alloc_buf(&payload);
    if buf.is_null() {
        return MIY_ERR_INVALID_ARG;
    }
    *out = buf;
    MIY_OK
}

unsafe extern "C" fn dispatch(
    state: *mut c_void,
    service: *const u8,
    service_len: usize,
    req: *const u8,
    req_len: usize,
    out: *mut *mut u8,
) -> i32 {
    catch_panic(
        || dispatch_inner(state, service, service_len, req, req_len, out),
        MIY_ERR_PANIC,
    )
}

static ENTRY: PluginEntryCell = PluginEntryCell(MiyPluginV1 {
    abi_version: MIY_ABI_VERSION,
    struct_size: core::mem::size_of::<MiyPluginV1>() as u32,
    id: ID.as_ptr() as *const c_char,
    version: VERSION.as_ptr() as *const c_char,
    start: Some(start),
    stop: Some(stop),
    dispatch: Some(dispatch),
});

#[no_mangle]
pub extern "C" fn miy_plugin_entry_v1() -> *const MiyPluginV1 {
    &ENTRY.0
}

// 让 `HashMap` 的导入不算未使用（清单缓存以后会用到）。
#[allow(dead_code)]
fn _cache_hint() -> HashMap<String, Value> {
    HashMap::new()
}

#[cfg(test)]
mod crash_tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("miy-crash-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join("crash-reports")).unwrap();
        std::fs::create_dir_all(p.join("logs")).unwrap();
        p
    }

    #[test]
    fn reads_description_from_newest_report() {
        let dir = tmpdir("desc");
        std::fs::write(
            dir.join("crash-reports").join("crash-2026-11-04_12.00.00-client.txt"),
            "---- Minecraft Crash Report ----\n// 哎呀\n\nTime: 2026-11-04\nDescription: Rendering overlay\n\njava.lang.IllegalStateException: boom\n",
        )
        .unwrap();
        std::fs::write(dir.join("logs").join("latest.log"), "line1\nline2\n").unwrap();
        let info = crash_info(dir.to_str().unwrap(), 1, None, None);
        assert_eq!(info.reason, "Rendering overlay");
        assert!(info.advice.is_empty());
        assert!(info.log.contains("line2"));
        assert!(info.crash.ends_with("crash-2026-11-04_12.00.00-client.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn falls_back_when_no_report() {
        let dir = tmpdir("none");
        let info = crash_info(dir.to_str().unwrap(), 1, None, None);
        assert!(info.reason.contains("退出代码"));
        assert!(info.crash.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rule_hits_through_crash_info() {
        let dir = tmpdir("oom");
        std::fs::write(
            dir.join("crash-reports").join("crash-2026-11-09_10.00.00-client.txt"),
            "---- Minecraft Crash Report ----\nDescription: Ticking entity\n\njava.lang.OutOfMemoryError: Java heap space\n",
        )
        .unwrap();
        let info = crash_info(dir.to_str().unwrap(), 1, Some(25), Some(21));
        assert_eq!(info.rule, "oom");
        assert!(info.advice.contains("内存"), "{}", info.advice);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn access_violation_beats_exit_code_text() {
        let dir = tmpdir("av");
        let info = crash_info(dir.to_str().unwrap(), -1073741819, None, None);
        assert_eq!(info.rule, "access_violation");
        assert!(info.advice.contains("显卡驱动"), "{}", info.advice);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod install_tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("miy-inst-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join("versions")).unwrap();
        p
    }

    #[test]
    fn empty_dir_is_not_usable() {
        let root = tmp("empty");
        let dir = root.join("versions").join("1.20.1");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!version_usable(&dir, "1.20.1", "1.20.1"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn vanilla_with_jar_is_usable() {
        let root = tmp("vanilla");
        let dir = root.join("versions").join("1.20.1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("1.20.1.json"), "{}").unwrap();
        std::fs::write(dir.join("1.20.1.jar"), b"x").unwrap();
        assert!(version_usable(&dir, "1.20.1", "1.20.1"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn loader_without_own_jar_uses_base_jar() {
        let root = tmp("loader");
        let base = root.join("versions").join("1.20.1");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("1.20.1.jar"), b"x").unwrap();
        let dir = root.join("versions").join("fabric-1.20.1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fabric-1.20.1.json"), "{}").unwrap();
        assert!(version_usable(&dir, "fabric-1.20.1", "1.20.1"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn incomplete_marker_wins() {
        let root = tmp("marker");
        let dir = root.join("versions").join("1.20.1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("1.20.1.json"), "{}").unwrap();
        std::fs::write(dir.join("1.20.1.jar"), b"x").unwrap();
        std::fs::write(dir.join(".incomplete"), "boom").unwrap();
        assert!(!version_usable(&dir, "1.20.1", "1.20.1"));
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod source_tests {
    use super::*;

    fn item(url: &str) -> Item {
        Item {
            path: "x".into(),
            url: url.into(),
            size: 0,
            sha1: None,
        }
    }

    /// 原版资源（客户端 jar、库、资源）只走 BMCLAPI 与 Mojang，绝不经过自建中转。
    #[test]
    fn vanilla_files_never_go_through_site() {
        let url = format!("{BMCLAPI}/assets/6a/6a84d4afab797aaf9f6ad283ebcd5899493c5c64");
        let c = candidates(&url);
        assert_eq!(c[0], url);
        assert!(c.iter().any(|u| u.starts_with(MOJANG_ASSETS)));
        assert!(!c.iter().any(|u| is_site(u)));
    }

    /// BMCLAPI 的 `/maven` 与 `/libraries` 指向同一批文件：两条都要给，
    /// 一条抽风时另一条通常还通，多给一条的成本几乎为零。
    #[test]
    fn library_has_both_mirror_paths() {
        let a = format!("{BMCLAPI}/maven/org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3.jar");
        let b = format!("{BMCLAPI}/libraries/org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3.jar");
        let c = candidates_for(&item(&a));
        assert!(c.iter().any(|u| u == &b), "缺 /libraries 回退：{c:?}");
        let c2 = candidates_for(&item(&b));
        assert!(c2.iter().any(|u| u == &a), "缺 /maven 回退：{c2:?}");
    }

    /// 客户端 jar 必须有官方地址，而且**排第一**：41 MB 要开 8 条连接分段，
    /// 镜像按节点限流会连续 429（实测白等一分钟），官方 CDN 没有这个限制。
    #[test]
    fn client_jar_prefers_official() {
        let sha = "e877b6a07acd633fb3bb475002175cec036e7b87";
        let it = Item {
            path: "versions/26.3/26.3.jar".into(),
            url: format!("{BMCLAPI}/version/26.3/client"),
            size: 41_483_720,
            sha1: Some(sha.into()),
        };
        let c = candidates_for(&it);
        let want = format!("https://piston-data.mojang.com/v1/objects/{sha}/client.jar");
        assert!(c.iter().any(|u| u == &want), "缺官方地址：{c:?}");
        assert_eq!(c[0], want, "官方应排第一：{c:?}");
        // 镜像仍然在候选里兜底。
        assert!(c.iter().any(|u| u.starts_with(BMCLAPI)), "缺镜像兜底：{c:?}");
    }

    /// 模组接口先走 MCIM，官方源第二，自建中转最后兜底。
    #[test]
    fn modrinth_uses_mcim_first() {
        let c = candidates(&format!("{MODRINTH}/search?query=sodium"));
        assert_eq!(c[0], format!("{MCIM}/modrinth/v2/search?query=sodium"));
        assert!(c.iter().any(|u| u.starts_with(MODRINTH)));
    }

    #[test]
    fn curseforge_uses_mcim_and_site() {
        let c = candidates(&format!("{MCIM}/curseforge/v1/mods/394468"));
        assert_eq!(c[0], format!("{MCIM}/curseforge/v1/mods/394468"));
        assert!(c.iter().any(|u| u.starts_with(CURSEFORGE)));
        assert!(c.iter().any(|u| is_site(u)));
    }

    /// `edge.forgecdn.net` 有镜像，`mediafilez.forgecdn.net` 没有（MCIM 文档明确说了）。
    #[test]
    fn forgecdn_mirrors_but_mediafilez_does_not() {
        let a = candidates_for(&item("https://edge.forgecdn.net/files/1234/5678/sodium.jar"));
        assert!(a
            .iter()
            .any(|u| u == &format!("{MCIM}/files/1234/5678/sodium.jar")));
        let b = candidates_for(&item("https://mediafilez.forgecdn.net/files/1234/5678/x.jar"));
        assert!(!b.iter().any(|u| u.starts_with(MCIM)));
        // 模组安装走的是 modrinth_urls，两条 CDN 都要有镜像兜底。
        assert!(modrinth_urls("https://edge.forgecdn.net/files/1234/5678/sodium.jar")
            .iter()
            .any(|u| u == &format!("{MCIM}/files/1234/5678/sodium.jar")));
        assert!(modrinth_urls("https://cdn.modrinth.com/data/abc/versions/1/x.jar")
            .iter()
            .any(|u| u == &format!("{MCIM}/data/abc/versions/1/x.jar")));
        assert_eq!(
            modrinth_urls("https://mediafilez.forgecdn.net/files/1/2/x.jar").len(),
            1
        );
    }

    /// 原版资源只走 BMCLAPI，模组接口只走 MCIM：两个 base 不能串。
    #[test]
    fn bases_point_at_the_right_hosts() {
        assert_eq!(mirror_base(), BMCLAPI);
        assert!(modrinth_base().starts_with(MCIM));
        assert!(curseforge_base().starts_with(MCIM));
    }

    #[test]
    fn mojang_mapping() {
        assert_eq!(
            mojang_of(&format!(
                "{BMCLAPI}/assets/6a/6a84d4afab797aaf9f6ad283ebcd5899493c5c64"
            ))
            .unwrap(),
            format!("{MOJANG_ASSETS}/6a/6a84d4afab797aaf9f6ad283ebcd5899493c5c64")
        );
        assert_eq!(
            mojang_of(&format!(
                "{BMCLAPI}/libraries/org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3.jar"
            ))
            .unwrap(),
            format!("{MOJANG_LIBS}/org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3.jar")
        );
        assert!(mojang_of(&format!("{BMCLAPI}/version/26.3/json")).is_none());
    }
}

//! 分块工作窃取下载引擎。
//!
//! 为什么不是「每个文件开 N 条连接」：那样连接数 = 文件数 × 分段数，几十个小文件
//! 同时下的时候会把连接池打满，反而更慢；而且一个大文件会把整批堵住。
//!
//! 这里的做法是**把文件切成块，所有块进同一个队列**，固定数量的工作线程去抢。
//! 好处有三个：
//! 1. 连接数有上界（= 工作线程数），不会因为文件多就爆炸；
//! 2. 慢连接只拖住自己那一块，抢得快的线程会继续吃后面的块；
//! 3. 失败只重试那一块，块小，重试便宜，也不会把已经下好的部分丢掉。
//!
//! 正确性上的几条硬规矩：
//! - 一律先写 `{path}.part`，**校验通过之后**才改名成正式文件；失败、取消都删掉半成品。
//! - 已存在的文件只有在「大小对得上、且（给了 sha1 时）哈希也对得上」才算下好。
//! - 服务器不认 `Range`（回 200 整包）时，这个文件退回单连接整包下载。
//! - 取消是协作式的：不再领新块、不再重试、不再降级，等在途写入结束再清理。

use crate::{call_json, HostCell};
use miy_abi::{MiyHostV1, MIY_LOG_DEBUG};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// 一个待下载的文件。
///
/// `urls` 是**候选源**，按优先级排列：中转站 → 镜像 → 官方。某一块失败时换下一个
/// 源重试，而不是对着同一个坏源死磕（HMCL/PCL2 的"多源回退"就是这个意思）。
pub struct Job {
    pub urls: Vec<String>,
    pub path: String,
    /// 期望大小；0 表示未知（不预分配、不校验大小）。
    pub size: u64,
    /// 可选的 sha1 校验（资源文件用得上）。
    pub sha1: Option<String>,
}

impl Job {
    /// 单源下载（Java 运行时、资源索引这类只有一个地址的文件）。
    #[allow(dead_code)]
    pub fn new(
        url: impl Into<String>,
        path: impl Into<String>,
        size: u64,
        sha1: Option<String>,
    ) -> Self {
        Self { urls: vec![url.into()], path: path.into(), size, sha1 }
    }

    /// 多个候选源，按优先级排列。
    pub fn with_sources(
        urls: Vec<String>,
        path: impl Into<String>,
        size: u64,
        sha1: Option<String>,
    ) -> Self {
        Self { urls, path: path.into(), size, sha1 }
    }

    /// 第 `i` 个候选源（循环取用）。现在主要走 `crate::pick_url`，留着给测试用。
    #[allow(dead_code)]
    fn url_at(&self, i: usize) -> &str {
        if self.urls.is_empty() {
            ""
        } else {
            &self.urls[i % self.urls.len()]
        }
    }
}

pub struct Report {
    /// 本次「有效」的字节数：已存在的文件 + 真正下下来的字节。
    pub bytes: u64,
    pub errors: Vec<String>,
    pub cancelled: bool,
    /// 成功落地的文件数（含本来就存在的）。
    pub done: usize,
}

/// 块大小上下限：太小请求开销大，太大重试代价高、进度也粗。
///
/// 上限给到 8 MiB 是因为镜像按「请求数」限流（实测 10 次/分钟/节点），
/// 一个 41 MB 的 jar 用 4 MiB 块要 11 次请求，用 8 MiB 只要 6 次。
const CHUNK_MIN: u64 = 256 << 10;
const CHUNK_MAX: u64 = 8 << 20;

/// 单个文件最多几条并发连接。镜像按单文件并发限速，超了就是 429。
///
/// 但「限速」是按连接算的：实测同一个 41 MB 的客户端 jar，1 条连接
/// 61 KB/s、8 条连接 4.4 MB/s（镜像和官方都是这个规律）。所以小文件
/// 保持 2 条（请求数少、不容易 429），大文件放开到 8 条 —— 真被限流了
/// 会走 `serial` 降级，退回一次一块。
const PER_FILE_MAX: usize = 2;
const PER_FILE_MAX_BIG: usize = 8;
const BIG_FILE: u64 = 16 << 20;

fn per_file_max(size: u64) -> usize {
    if size >= BIG_FILE {
        PER_FILE_MAX_BIG
    } else {
        PER_FILE_MAX
    }
}

/// 单块最多试几次（含第一次）。
const RETRIES: usize = 8;

const ST_PENDING: u8 = 0;
const ST_NORANGE: u8 = 1;
const ST_FAILED: u8 = 2;
const ST_DONE: u8 = 3;

// ---- 全局连接预算 ---------------------------------------------------------
//
// PCL2 的做法是「全局线程池 + 按实测速度动态加线程」。我们保留每批的工作
// 线程，但把**同时在飞的请求数**收归全局：两个批次不会各开一份，慢文件也
// 占不满名额。上限按实测速度自适应（PCL2 用的就是 85% 这条线）。

/// 一批最多开多少工作线程（真正在飞的数量由预算决定，通常远小于它）。
const MAX_WORKERS: usize = 16;

struct Budget {
    /// 当前允许的在飞请求数。
    limit: usize,
    /// 已经在飞的请求数。
    in_use: usize,
    /// 历史最好速度（B/s），用来判断"是不是没跑满"。
    best_bps: u64,
    last_bytes: u64,
    last_ms: u64,
    /// 被限流后压到 1 条连接的截止时刻。
    floor_until: u64,
    /// 最近一次 429 的时刻（留着判断"是不是还在被限流"）。
    last_429: u64,
}

static BUDGET: Mutex<Budget> = Mutex::new(Budget {
    limit: 4,
    in_use: 0,
    best_bps: 0,
    last_bytes: 0,
    last_ms: 0,
    floor_until: 0,
    last_429: 0,
});

/// 把全局上限抬到至少 `n`（多个批次取最大值，谁也不许把别人的额度抢走）。
fn budget_raise(n: usize) {
    let Ok(mut b) = BUDGET.lock() else { return };
    let n = n.clamp(1, MAX_WORKERS);
    if n > b.limit {
        b.limit = n;
    }
}

/// 领一个连接名额。取消时返回 false（不再领新的）。
fn budget_acquire(cancel: Option<&AtomicBool>) -> bool {
    loop {
        let Ok(mut b) = BUDGET.lock() else { return false };
        let now = now_ms();
        let limit = if now < b.floor_until { 1 } else { b.limit };
        if b.in_use < limit {
            b.in_use += 1;
            return true;
        }
        let wait = if now < b.floor_until {
            Duration::from_millis((b.floor_until - now).min(300))
        } else {
            Duration::from_millis(40)
        };
        drop(b);
        if cancel.map(|c| c.load(Ordering::SeqCst)).unwrap_or(false) {
            return false;
        }
        std::thread::sleep(wait);
    }
}

fn budget_release() {
    if let Ok(mut b) = BUDGET.lock() {
        b.in_use = b.in_use.saturating_sub(1);
    }
}

/// 被限流：短暂退让，别把整个批次冻住。
///
/// 真正的等待交给 `rl_wait()` —— 它读服务器给的 `Reset-short`，比我们猜的准。
/// 这里只让刚撞墙的那一小会儿别再挤。
fn budget_429(reset_ms: u64) {
    if let Ok(mut b) = BUDGET.lock() {
        b.floor_until = now_ms() + reset_ms.clamp(500, 3_000);
        b.last_429 = now_ms();
    }
}

/// 每 250 ms 调一次，只记速度，**不自动加并发**。
///
/// 试过 PCL2 的「低于历史最好 85% 就加一条」：在这台机器上反而更慢
/// （103 s → 262 s，429 从 4 次涨到 12 次）。原因是镜像按节点配额限流，
/// 速度掉下来往往正是被限流的信号，这时候加并发等于往墙上撞。
/// 所以并发只由设置决定，全局预算只负责「跨批次共享同一个上限」。
fn budget_tick(bytes_now: u64) {
    let Ok(mut b) = BUDGET.lock() else { return };
    let now = now_ms();
    if b.last_ms == 0 {
        b.last_ms = now;
        b.last_bytes = bytes_now;
        return;
    }
    let dt = now.saturating_sub(b.last_ms);
    if dt < 900 {
        return;
    }
    let db = bytes_now.saturating_sub(b.last_bytes);
    let bps = db.saturating_mul(1000) / dt.max(1);
    b.last_ms = now;
    b.last_bytes = bytes_now;
    if bps > b.best_bps {
        b.best_bps = bps;
    }
}

// ---- 错误分类 -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 服务器回了非 2xx。
    Http(u16),
    /// 服务器不支持 Range（回 200 整包）。
    NoRange,
    /// 连接层失败（超时、断连、DNS…）。
    Net,
}

#[derive(Debug, Clone)]
struct DlError {
    kind: Kind,
    msg: String,
    /// 服务器给的 `Retry-After`（秒）。
    retry_after: Option<u64>,
}

impl DlError {
    fn new(kind: Kind, msg: impl Into<String>) -> Self {
        Self { kind, msg: msg.into(), retry_after: None }
    }

    /// 值得再试一次吗？4xx（除 408/429）是永久错误，重试只是浪费时间。
    fn retryable(&self) -> bool {
        match self.kind {
            Kind::Http(code) => code == 408 || code == 429 || (500..600).contains(&code),
            Kind::Net => true,
            _ => false,
        }
    }

    /// 第 `attempt` 次重试前等多久：优先听服务器的，其次指数退避 + 抖动。
    fn backoff(&self, attempt: usize) -> Duration {
        if let Some(sec) = self.retry_after {
            return Duration::from_millis(sec.saturating_mul(1000).min(30_000));
        }
        let base: u64 = match self.kind {
            Kind::Http(429) => 5000,
            Kind::Http(_) => 500,
            _ => 300,
        };
        let ms = base.saturating_mul(1u64 << attempt.min(4));
        // 抖动：别让所有线程在同一毫秒一起回来再撞一次。
        let jitter = now_ms() % 250;
        Duration::from_millis((ms + jitter).min(20_000))
    }
}

/// 把宿主返回的 `{ok:false,...}` 翻成结构化错误。
fn parse_error(v: &Value) -> DlError {
    let status = v.get("status").and_then(Value::as_u64).map(|s| s as u16);
    let retry_after = v.get("retry_after").and_then(Value::as_u64);
    let msg = v
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("下载失败")
        .to_string();
    let kind = if v.get("kind").and_then(Value::as_str) == Some("norange")
        || msg.contains("不支持分段")
    {
        Kind::NoRange
    } else if let Some(code) = status {
        Kind::Http(code)
    } else if let Some(rest) = msg.strip_prefix("HTTP ") {
        // 老版本宿主只给字符串，兜底解析一下。解析不出来按 5xx 处理 ——
        // 当成 0 会被 `retryable()` 判成永久错误，把网络抖动变成硬失败。
        Kind::Http(rest.trim().parse().unwrap_or(500))
    } else {
        Kind::Net
    };
    DlError { kind, msg, retry_after }
}

// ---- 限流：按站点分别记 ---------------------------------------------------

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn host_of(url: &str) -> String {
    let host = url
        .split("://")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .unwrap_or("");
    // 去掉 `user:pass@` 与 `:port`：限流按站点算，凭据和端口不该把同一个站点拆成几个。
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    host.to_ascii_lowercase()
}

fn throttles() -> &'static Mutex<std::collections::HashMap<String, u64>> {
    static MAP: OnceLock<Mutex<std::collections::HashMap<String, u64>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// 每个站点一份限速闸门：`(下次允许发请求的时刻, 当前惩罚毫秒)`。
///
/// 为什么需要它：镜像按 IP 限流，我们这边几十条连接各自退避、各自重试，只会把
/// 冷却时间越推越长（实测一次 124 MB 的安装被 429 拖到 8 分钟）。改成同一站点的
/// 线程共用一个闸门：撞到 429 就把闸门往后推、惩罚翻倍；成功一次就减半。
///
/// **必须按站点分开**：以前是全局一份，镜像回一句「剩余 0，60 秒后重置」，
/// 连自建 CDN 和官方源的请求也一起冻住 —— 那正是「卡在 99% 不动」的主因。
fn gates() -> &'static Mutex<std::collections::HashMap<String, (u64, u64)>> {
    static G: OnceLock<Mutex<std::collections::HashMap<String, (u64, u64)>>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn note_limited(url: &str) {
    let host = host_of(url);
    if let Ok(mut m) = throttles().lock() {
        m.insert(host.clone(), now_ms());
    }
    // 服务器自报了窗口（`Remaining-short: 0` + `Reset-short: 60`）就照它等，
    // 别用指数退避瞎猜：实测盲退避是 5/10/20 秒，而窗口其实是 60 秒，
    // 于是每个块都要白撞 4 次 429 才成功。
    let window = RL.lock().ok().and_then(|r| r.0).filter(|(rem, _)| *rem == 0);
    if let Ok(mut m) = gates().lock() {
        let g = m.entry(host).or_insert((0, 0));
        match window {
            Some((_, reset)) => {
                g.1 = (reset as u64 * 1000).min(60_000);
                g.0 = now_ms() + g.1;
            }
            None => {
                g.1 = (g.1.saturating_mul(2)).saturating_add(1000).min(30_000);
                g.0 = now_ms() + g.1;
            }
        }
    }
}

fn note_ok(url: &str) {
    if let Ok(mut m) = gates().lock() {
        let g = m.entry(host_of(url)).or_insert((0, 0));
        g.1 /= 2;
        g.0 = now_ms() + 40;
    }
}

/// 等到这个站点的闸门放行。分段小睡，取消时能及时退出。
fn throttle_wait(url: &str) {
    let host = host_of(url);
    loop {
        let next = gates()
            .lock()
            .ok()
            .and_then(|m| m.get(&host).map(|g| g.0))
            .unwrap_or(0);
        let now = now_ms();
        if now >= next {
            return;
        }
        std::thread::sleep(Duration::from_millis((next - now).min(1000)));
    }
}

/// 服务器自报的额度：`(剩余, 重置秒数)`，短窗口与长窗口各一份。
static RL: Mutex<(Option<(u32, u32)>, Option<(u32, u32)>)> = Mutex::new((None, None));

/// 客户端令牌桶：把服务器自报的额度当成权威，请求前先扣一个。
///
/// 之前是「看到剩余 ≤1 才睡」—— 4 条线程同时看到 rem=3，就一起发出去，
/// 结果照样 429。这里把「检查 + 扣减」放进同一把锁，谁也别抢跑。
struct Bucket {
    tokens: u32,
    /// 窗口重置时刻（毫秒时间戳）。
    reset_at: u64,
    /// 一个窗口给多少（服务器没报就用 10）。
    limit: u32,
}

/// 每个站点一份令牌桶。
///
/// 以前是全局一份：镜像回一句「剩余 0，60 秒后重置」，连官方源
/// （libraries.minecraft.net / piston-data）的请求也一起排队 —— 实测装一个
/// 新版本 77 个文件被拖成 8 分钟，日志里每 60 秒才落一个文件。额度是
/// **按站点**算的，官方源根本不该受镜像的牵连。
fn buckets() -> &'static Mutex<std::collections::HashMap<String, Bucket>> {
    static M: OnceLock<Mutex<std::collections::HashMap<String, Bucket>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn note_rl(url: &str, v: &Value) {
    let pair = |k: &str| -> Option<(u32, u32)> {
        let a = v.get(k)?.as_array()?;
        Some((a.first()?.as_u64()? as u32, a.get(1)?.as_u64()? as u32))
    };
    let (s, l) = (pair("rl_short"), pair("rl_long"));
    if s.is_none() && l.is_none() {
        return;
    }
    if let Ok(mut r) = RL.lock() {
        if s.is_some() {
            r.0 = s;
        }
        if l.is_some() {
            r.1 = l;
        }
    }
    // 短窗口才是真正卡人的那个：直接照抄服务器的剩余量。
    if let Some((rem, reset)) = s {
        if let Ok(mut m) = buckets().lock() {
            let b = m.entry(host_of(url)).or_insert(Bucket {
                tokens: 10,
                reset_at: 0,
                limit: 10,
            });
            b.tokens = rem;
            b.reset_at = now_ms() + reset as u64 * 1000;
        }
    }
}

/// 请求前领一个额度；没有就等到窗口重置。
///
/// BMCLAPI 的短窗口是「10 次 / 15 秒」，长窗口「1000 次 / 约 55 分钟」。盲撞的话
/// 每个块都要吃 429、白等几十秒；照着额度排队，同样的文件能快好几倍。
fn rl_wait(url: &str) {
    let host = host_of(url);
    loop {
        let wait = {
            let Ok(mut m) = buckets().lock() else { return };
            // 没自报过额度的站点（官方源、自建 CDN）不排队。
            let Some(b) = m.get_mut(&host) else { return };
            let now = now_ms();
            if now >= b.reset_at {
                b.tokens = b.limit;
                b.reset_at = now + 60_000;
            }
            if b.tokens > 0 {
                b.tokens -= 1;
                return;
            }
            b.reset_at.saturating_sub(now).clamp(50, 2_000)
        };
        std::thread::sleep(Duration::from_millis(wait));
    }
}

// ---- 流式 SHA-1 -----------------------------------------------------------

struct Sha1 {
    h: [u32; 5],
    buf: [u8; 64],
    n: usize,
    len: u64,
}

impl Sha1 {
    fn new() -> Self {
        Self {
            h: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0],
            buf: [0u8; 64],
            n: 0,
            len: 0,
        }
    }

    fn block(&mut self, chunk: &[u8]) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) =
            (self.h[0], self.h[1], self.h[2], self.h[3], self.h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        self.h[0] = self.h[0].wrapping_add(a);
        self.h[1] = self.h[1].wrapping_add(b);
        self.h[2] = self.h[2].wrapping_add(c);
        self.h[3] = self.h[3].wrapping_add(d);
        self.h[4] = self.h[4].wrapping_add(e);
    }

    fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        if self.n > 0 {
            let take = (64 - self.n).min(data.len());
            self.buf[self.n..self.n + take].copy_from_slice(&data[..take]);
            self.n += take;
            data = &data[take..];
            if self.n == 64 {
                let block = self.buf;
                self.block(&block);
                self.n = 0;
            }
        }
        while data.len() >= 64 {
            let (head, tail) = data.split_at(64);
            self.block(head);
            data = tail;
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.n = data.len();
        }
    }

    fn finish(mut self) -> String {
        let bits = self.len.wrapping_mul(8);
        let mut tail = vec![0x80u8];
        while (self.n + tail.len()) % 64 != 56 {
            tail.push(0);
        }
        tail.extend_from_slice(&bits.to_be_bytes());
        self.update(&tail);
        self.h.iter().map(|x| format!("{x:08x}")).collect()
    }
}

/// 流式算文件 sha1：大文件不整个读进内存；打不开、读失败都算校验失败。
fn sha1_file(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("打开文件失败：{e}"))?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 256 << 10];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("读取文件失败：{e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finish())
}

// ---- 断点续传日志 ---------------------------------------------------------
//
// 为什么需要它：一次 462 MB 的资源下载跑到 90% 被取消/断网，没有日志就只能从头再来。
// 日志是纯文本，一行 `偏移 长度`，追加写；文件校验通过后整份删掉。
// 只记录"已经写进 .part 的区间"，所以日志本身坏了最多是重下几块，不会写出坏文件。

fn journal_path(path: &str) -> PathBuf {
    PathBuf::from(format!("{path}.ranges"))
}

fn load_journal(path: &Path) -> Vec<(u64, u64)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out: Vec<(u64, u64)> = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(a), Some(b)) = (it.next(), it.next()) else { continue };
        let (Ok(off), Ok(len)) = (a.parse::<u64>(), b.parse::<u64>()) else { continue };
        if len > 0 {
            out.push((off, len));
        }
    }
    out.sort_unstable();
    out
}

/// `[0, size)` 减去已完成的区间，得到还缺的区间。
fn missing_ranges(size: u64, done: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut cur = 0u64;
    for &(off, len) in done {
        let end = off.saturating_add(len).min(size);
        if off > cur {
            out.push((cur, off - cur));
        }
        cur = cur.max(end);
        if cur >= size {
            break;
        }
    }
    if cur < size {
        out.push((cur, size - cur));
    }
    out
}

fn append_journal(path: &Path, off: u64, len: u64) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{off} {len}");
    }
}

// ---- 每个文件的状态 -------------------------------------------------------

struct Slot {
    job: Job,
    /// 它在 `jobs` 里的原始下标。回调必须用它，不能用 slots 下标 ——
    /// 一旦有文件被跳过，两者就错位了。
    orig: usize,
    part: PathBuf,
    /// 断点续传日志：每下完一块追加一行 `偏移 长度`，下次启动据此跳过。
    journal: PathBuf,
    state: AtomicU8,
    /// 还没下完的区间（偏移, 长度）。用队列而不是单调游标，是为了支持
    /// 断点续传（区间可能不连续）和尾部对冲（把大尾巴劈成两半）。
    pending: Mutex<VecDeque<(u64, u64)>>,
    /// 当前优先用第几个候选源。
    src: AtomicUsize,
    /// 已领走但还没写完的块数。
    left: AtomicUsize,
    /// 被限流过：改成一次只跑一块，块也放大 —— 请求数少了才不会被继续 429。
    serial: AtomicBool,
    /// 第一次撞到 429 的时刻（毫秒）。限流不是文件的错，块会放回队列等窗口；
    /// 但也不能无限等下去，超过 3 分钟就判失败，免得用户看着 99% 干等。
    rl_since: AtomicU64,
    /// 已写入的字节数。
    done: AtomicU64,
    /// 最近一次有进展的时刻（毫秒），用来发现「卡死在某个文件上」。
    last_ms: AtomicU64,
    /// 最后一块是从哪个源下下来的。排障时最想知道的就是这个：
    /// 「这个文件到底走的 CDN、镜像还是官方」。
    last_src: Mutex<String>,
    finalized: Mutex<bool>,
    err: Mutex<Option<String>>,
}

impl Slot {
    fn state(&self) -> u8 {
        self.state.load(Ordering::SeqCst)
    }

    fn set_state(&self, s: u8) {
        self.state.store(s, Ordering::SeqCst);
    }

    fn fail(&self, msg: String) {
        if let Ok(mut e) = self.err.lock() {
            if e.is_none() {
                *e = Some(msg);
            }
        }
        self.set_state(ST_FAILED);
    }
}

/// 校验并落地：大小对、sha1 对，才把 `.part` 改名成正式文件。
fn finalize(slot: &Slot) -> Result<(), String> {
    let mut done = slot.finalized.lock().map_err(|_| "状态锁坏了")?;
    if *done {
        return Ok(());
    }
    let part = &slot.part;
    if slot.job.size > 0 {
        let len = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
        if len != slot.job.size {
            let _ = std::fs::remove_file(part);
            return Err(format!("{} 大小不对（{len} / {}）", slot.job.path, slot.job.size));
        }
    }
    if let Some(want) = &slot.job.sha1 {
        let got = sha1_file(part)?;
        if !got.eq_ignore_ascii_case(want) {
            let _ = std::fs::remove_file(part);
            return Err(format!("{} 校验失败", slot.job.path));
        }
    }
    if let Some(dir) = Path::new(&slot.job.path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Windows 上 rename 不会覆盖已存在的目标（"目标文件存在但校验不过 → 重下"
    // 正是常见路径），所以先删掉旧的再改名。
    let _ = std::fs::remove_file(&slot.job.path);
    if let Err(e) = std::fs::rename(part, &slot.job.path) {
        let _ = std::fs::remove_file(&slot.job.path);
        std::fs::rename(part, &slot.job.path).map_err(|e2| format!("改名失败：{e} / {e2}"))?;
    }
    *done = true;
    // 落地成功，续传日志没用了；留着反而会让下次误以为还缺块。
    let _ = std::fs::remove_file(&slot.journal);
    Ok(())
}

/// 跑一批下载。
///
/// `workers` 是并发连接数（会被夹到 1..=64）；`on_tick` 每 250 ms 被调用一次，
/// 参数是「已完成字节 / 总字节」，用来刷进度条；`on_job` 在某个文件**校验通过**
/// 之后调用一次，参数是它在 `jobs` 里的原始下标。
pub fn run(
    h: &MiyHostV1,
    jobs: Vec<Job>,
    workers: usize,
    cancel: Option<&AtomicBool>,
    on_tick: &(dyn Fn(u64, u64) + Sync),
    on_job: &(dyn Fn(usize) + Sync),
) -> Report {
    let workers = workers.clamp(1, 64);
    // 全局预算至少给到这一批要的数量；别的批次只会把它抬高，不会压低。
    budget_raise(workers);
    let mut errors: Vec<String> = Vec::new();
    let want_stop = || cancel.map(|c| c.load(Ordering::SeqCst)).unwrap_or(false);

    // 1) 先看哪些文件其实已经下好了（大小对 + 有 sha1 时哈希也对）。
    let mut slots: Vec<Slot> = Vec::with_capacity(jobs.len());
    let mut skipped_bytes: u64 = 0;
    let mut skipped: Vec<usize> = Vec::new();
    for (i, job) in jobs.into_iter().enumerate() {
        let path = PathBuf::from(&job.path);
        let mut ok = false;
        if job.size > 0 {
            if let Ok(meta) = std::fs::metadata(&path) {
                if meta.len() == job.size {
                    match &job.sha1 {
                        Some(want) => match sha1_file(&path) {
                            Ok(got) => ok = got.eq_ignore_ascii_case(want),
                            Err(_) => ok = false,
                        },
                        None => ok = true,
                    }
                }
            }
        }
        if ok {
            skipped_bytes += job.size;
            skipped.push(i);
            continue;
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let part = PathBuf::from(format!("{}.part", job.path));
        let journal = journal_path(&job.path);
        // 断点续传：上次留下的日志告诉我们哪些区间已经在 .part 里了。
        // 只有 .part 还在、且大小对得上时才敢信日志，否则日志作废重下。
        let mut pending: VecDeque<(u64, u64)> = VecDeque::new();
        if job.size > 0 {
            let part_ok = std::fs::metadata(&part)
                .map(|m| m.len() == job.size)
                .unwrap_or(false);
            let done = if part_ok { load_journal(&journal) } else { Vec::new() };
            if !part_ok {
                let _ = std::fs::remove_file(&journal);
            }
            for r in missing_ranges(job.size, &done) {
                pending.push_back(r);
            }
        }
        slots.push(Slot {
            job,
            orig: i,
            part,
            journal,
            state: AtomicU8::new(ST_PENDING),
            pending: Mutex::new(pending),
            src: AtomicUsize::new(0),
            left: AtomicUsize::new(0),
            serial: AtomicBool::new(false),
            rl_since: AtomicU64::new(0),
            done: AtomicU64::new(0),
            last_ms: AtomicU64::new(now_ms()),
            last_src: Mutex::new(String::new()),
            finalized: Mutex::new(false),
            err: Mutex::new(None),
        });
    }

    // 已经存在的文件也要回调，规则和真正下完的一致。
    for i in &skipped {
        on_job(*i);
    }

    let total_known: u64 = slots.iter().map(|s| s.job.size).sum::<u64>() + skipped_bytes;
    let done_bytes = AtomicU64::new(skipped_bytes);
    let extra_total = AtomicU64::new(0); // 大小未知的文件下完才知道总量
    let stopped = AtomicBool::new(false);
    let cell = HostCell(*h);

    if !slots.is_empty() && !want_stop() {
        // 2) 预分配 `.part`：多线程写不同偏移，长度必须先定下来。
        for s in &mut slots {
            if s.job.size == 0 {
                continue;
            }
            match std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&s.part)
            {
                Ok(f) => {
                    if let Err(e) = f.set_len(s.job.size) {
                        s.fail(format!("占位失败：{e}"));
                    }
                }
                Err(e) => s.fail(format!("打开文件失败：{e}")),
            }
        }

        let cursor = AtomicUsize::new(0);
        let ticker_stop = AtomicBool::new(false);
        let slots_ref = &slots;
        let done_ref = &done_bytes;
        let extra_ref = &extra_total;
        let stopped_ref = &stopped;

        std::thread::scope(|scope| {
            // 进度 + 看门狗 + 尾部对冲
            scope.spawn(|| {
                while !ticker_stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(250));
                    let total = total_known + extra_ref.load(Ordering::Relaxed);
                    on_tick(done_ref.load(Ordering::Relaxed), total);
                    // 自适应：速度没跑满就加一条连接（全局预算，跨批次共享）。
                    budget_tick(done_ref.load(Ordering::Relaxed));

                    let now = now_ms();
                    // 尾部对冲：只剩一个大尾巴、且 3 秒没进展时，把它劈成两半，
                    // 让两条连接一起抢（Axolotl 的 tail hedge 就是这个思路）。
                    // 劈到 2 MiB 以下就停，免得碎成一地小请求。
                    for s in slots_ref {
                        if s.state() != ST_PENDING {
                            continue;
                        }
                        if now.saturating_sub(s.last_ms.load(Ordering::Relaxed)) < 3_000 {
                            continue;
                        }
                        let Ok(mut q) = s.pending.lock() else { continue };
                        let Some(pos) = q.iter().position(|&(_, len)| len >= 2 << 20) else {
                            continue;
                        };
                        let (off, len) = q[pos];
                        let half = len / 2;
                        q.remove(pos);
                        q.push_front((off + half, len - half));
                        q.push_front((off, half));
                    }

                    // 卡死看门狗：某个文件长时间没有任何字节进展，就把它判失败，
                    // 让整批继续往下走，而不是永远等在那一条连接上。
                    // 阈值必须大于「单块重试总时长」（RETRIES × 30 s + 退避），
                    // 否则正常的慢下载会被误杀。
                    for s in slots_ref {
                        if s.state() == ST_PENDING
                            && s.left.load(Ordering::Relaxed) > 0
                            && now.saturating_sub(s.last_ms.load(Ordering::Relaxed)) > 240_000
                        {
                            s.fail(format!("{} 下载超时，已跳过", s.job.path));
                        }
                    }
                }
            });

            let mut handles = Vec::with_capacity(MAX_WORKERS);
            for _ in 0..MAX_WORKERS {
                let cell = &cell;
                let cursor = &cursor;
                let slots = slots_ref;
                let done_bytes = done_ref;
                let stopped = stopped_ref;
                handles.push(scope.spawn(move || loop {
                    if want_stop() || stopped.load(Ordering::Relaxed) {
                        stopped.store(true, Ordering::Relaxed);
                        break;
                    }
                    // 真正在飞的数量由全局预算决定：线程多开几个没关系，
                    // 拿不到名额就在这里等，不会把镜像打爆。
                    if !budget_acquire(cancel) {
                        break;
                    }

                    // 轮流领：从游标位置开始找第一个还有待下区间的文件。
                    let n = slots.len();
                    let start = cursor.fetch_add(1, Ordering::Relaxed);
                    let mut claimed: Option<(usize, u64, u64)> = None;
                    for k in 0..n {
                        let i = (start + k) % n;
                        let s = &slots[i];
                        if s.state() != ST_PENDING || s.job.size == 0 {
                            continue;
                        }
                        // 同一个文件最多几条连接：小文件 2 条，大文件 8 条。
                        // 镜像节点是按「单文件并发」限速的，但限的是每条连接，
                        // 所以大文件多开几条才跑得满带宽；真 429 了会走 serial。
                        if s.left.load(Ordering::Relaxed) >= per_file_max(s.job.size) {
                            continue;
                        }
                        // 被限流过的文件改成串行：一次只跑一块，块也放大到 8 MiB，
                        // 请求数少一半，才不会被继续 429。
                        let serial = s.serial.load(Ordering::Relaxed);
                        if serial && s.left.load(Ordering::Relaxed) > 0 {
                            continue;
                        }
                        let want = if serial {
                            (8u64 << 20).min(CHUNK_MAX)
                        } else {
                            (s.job.size / (workers as u64 * 3)).clamp(CHUNK_MIN, CHUNK_MAX)
                        };
                        let Ok(mut q) = s.pending.lock() else { continue };
                        let Some((off, len)) = q.pop_front() else { continue };
                        if len > want {
                            // 大区间切成小块，剩下的塞回队首，保证顺序推进。
                            q.push_front((off + want, len - want));
                            claimed = Some((i, off, want));
                        } else {
                            claimed = Some((i, off, len));
                        }
                        drop(q);
                        if claimed.is_some() {
                            break;
                        }
                    }
                    let Some((i, off, len)) = claimed else {
                        budget_release();
                        break; // 所有文件都没有待领的块了
                    };
                    let s = &slots[i];
                    let job = &s.job;
                    s.left.fetch_add(1, Ordering::SeqCst);

                    let mut last: Option<DlError> = None;
                    let mut ok = false;
                    // 撞到 429 时把这一块放回队列、等窗口重置，而不是判这个文件死刑。
                    let mut requeued = false;
                    for attempt in 0..RETRIES {
                        // 看门狗或别的线程可能已经判它失败了，别再白试。
                        if s.state() != ST_PENDING {
                            break;
                        }
                        if attempt > 0 {
                            let wait = last
                                .as_ref()
                                .map(|e| e.backoff(attempt))
                                .unwrap_or(Duration::from_millis(300));
                            std::thread::sleep(wait);
                            if want_stop() || stopped.load(Ordering::Relaxed) {
                                break;
                            }
                        }
                        // 换源：每次重试换下一个候选源，别对着同一个坏源死磕。
                        let src = s.src.load(Ordering::Relaxed);
                        let url = crate::pick_url(&job.urls, src + attempt);
                        throttle_wait(url);
                        rl_wait(url);
                        let req = json!({
                            "url": url,
                            "path": s.part.to_string_lossy(),
                            "offset": off,
                            "length": len,
                            "timeout_ms": 30_000u64
                        });
                        match call_json(cell.h(), "miy.net.download_range", &req) {
                            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                                note_rl(url, &v);
                                // 宿主说成功还不够：写进去的字节数必须等于这一块的长度。
                                // 否则预分配留下的 0 会变成文件里的洞，而 size 校验恒真、看不出来。
                                let wrote = v.get("bytes").and_then(Value::as_u64);
                                if wrote != Some(len) {
                                    last = Some(DlError::new(
                                        Kind::Net,
                                        format!(
                                            "{} 偏移 {off} 只写了 {} / {len} 字节",
                                            job.path,
                                            wrote
                                                .map(|n| n.to_string())
                                                .unwrap_or_else(|| "?".to_string())
                                        ),
                                    ));
                                    continue;
                                }
                                s.done.fetch_add(len, Ordering::Relaxed);
                                s.last_ms.store(now_ms(), Ordering::Relaxed);
                                done_bytes.fetch_add(len, Ordering::Relaxed);
                                note_ok(url);
                                // 记下这一块是哪个源给的，排障时一眼看出走的 CDN 还是镜像。
                                if let Ok(mut src) = s.last_src.lock() {
                                    *src = host_of(url).to_string();
                                }
                                // 记进续传日志：下次启动跳过这一块。
                                append_journal(&s.journal, off, len);
                                ok = true;
                                break;
                            }
                            Ok(v) => {
                                note_rl(url, &v);
                                let e = parse_error(&v);
                                if e.kind == Kind::NoRange {
                                    // 这个源不认 Range（实测某个节点对 Range 请求回 200 整包）：
                                    // 记下来，后面的块别再往它身上撞。
                                    crate::mark_no_range(url);
                                }
                                if e.kind == Kind::Http(429) {
                                    note_limited(url);
                                    // 被限流了：这个文件改成串行 + 大块，别再拿多条连接去撞。
                                    // 不退回整包 —— 整包是一条长连接，被限流时同样卡，
                                    // 而且会丢掉已经下好的块。闸门会替我们等。
                                    s.serial.store(true, Ordering::Relaxed);
                                    // 限流是按节点的：短暂退让 + 砍半并发，
                                    // 真正的等待交给 rl_wait()（读服务器给的 Reset-short）。
                                    budget_429(2_000);
                                    // 限流不是这个文件的错：把这一块放回队首，等窗口重置后重领。
                                    // 以前这里直接 s.fail()，一个 429 就把整个文件判死 ——
                                    // 表现就是进度卡在 99%、装完缺文件。
                                    if let Ok(mut q) = s.pending.lock() {
                                        q.push_front((off, len));
                                    }
                                    let since = s.rl_since.load(Ordering::Relaxed);
                                    if since == 0 {
                                        s.rl_since.store(now_ms(), Ordering::Relaxed);
                                    } else if now_ms().saturating_sub(since) > 180_000 {
                                        // 等了 3 分钟还在被限流：认输，报清楚原因。
                                        last = Some(DlError::new(
                                            Kind::Http(429),
                                            format!("{} 被限流超过 3 分钟", job.path),
                                        ));
                                        break;
                                    }
                                    requeued = true;
                                    break;
                                }
                                // 还有别的源就换一个再试：4xx 对当前源是永久的，
                                // 对下一个源未必（镜像没同步、官方有）。
                                if job.urls.len() > 1 {
                                    s.src.fetch_add(1, Ordering::Relaxed);
                                }
                                let fatal = !e.retryable() && job.urls.len() <= 1;
                                last = Some(e);
                                if fatal {
                                    break;
                                }
                            }
                            Err(msg) => {
                                last = Some(DlError::new(Kind::Net, msg));
                            }
                        }
                    }

                    if !ok && !requeued {
                        let e = last.unwrap_or_else(|| DlError::new(Kind::Net, "下载失败"));
                        if e.kind == Kind::NoRange {
                            // 这个文件整体退回单连接，别的块别再试了。
                            s.set_state(ST_NORANGE);
                        } else if e.kind == Kind::Http(429) {
                            s.fail("镜像限流，请稍后重试".to_string());
                        } else {
                            s.fail(e.msg);
                        }
                    }

                    // 这一块结束了：如果它是这个文件的最后一块，就校验落地。
                    if s.left.fetch_sub(1, Ordering::SeqCst) == 1
                        && s.state() == ST_PENDING
                        && s.pending.lock().map(|q| q.is_empty()).unwrap_or(false)
                    {
                        match finalize(s) {
                            Ok(()) => {
                                s.set_state(ST_DONE);
                                if let Ok(src) = s.last_src.lock() {
                                    if !src.is_empty() {
                                        unsafe {
                                            cell.0.log_str(
                                                MIY_LOG_DEBUG,
                                                &format!("下载完成 {}（来自 {src}）", s.job.path),
                                            );
                                        }
                                    }
                                }
                                on_job(s.orig);
                            }
                            Err(msg) => s.fail(msg),
                        }
                    }
                    // 这一块彻底结束，把名额还回去。
                    budget_release();
                }));
            }

            // 先等工作线程全部退出，再让进度线程收工 —— 反过来会互相等（死锁）。
            for (i, h) in handles.into_iter().enumerate() {
                if h.join().is_err() {
                    // 线程 panic 了：它手上的文件不能算完成。
                    for s in slots_ref {
                        if s.state() == ST_PENDING {
                            s.fail(format!("下载线程 {i} 异常退出"));
                        }
                    }
                }
            }
            ticker_stop.store(true, Ordering::Relaxed);
        });
    }

    let cancelled = stopped.load(Ordering::Relaxed) || want_stop();

    // 3) 不支持 Range 的文件：整包重下一次（取消时不再开始新的）。
    for s in slots.iter() {
        if cancelled {
            break;
        }
        if s.state() != ST_NORANGE {
            continue;
        }
        let _ = std::fs::remove_file(&s.part);
        // 之前成功的块已经计过字节了，整包重下会把它们重复计一遍。
        done_bytes.fetch_sub(s.done.load(Ordering::Relaxed), Ordering::Relaxed);
        s.done.store(0, Ordering::Relaxed);
        match crate::download_any(h, &s.job.urls, &s.job.path, s.job.size) {
            Ok(n) => {
                s.done.store(n, Ordering::Relaxed);
                done_bytes.fetch_add(n, Ordering::Relaxed);
                if s.job.size == 0 {
                    extra_total.fetch_add(n, Ordering::Relaxed);
                }
                // 整包下载也要过一遍校验，规则和分块路径一致。
                let check = if s.job.size > 0 {
                    std::fs::metadata(&s.job.path).map(|m| m.len()).unwrap_or(0) == s.job.size
                } else {
                    true
                };
                let hash_ok = match &s.job.sha1 {
                    Some(want) => sha1_file(Path::new(&s.job.path))
                        .map(|got| got.eq_ignore_ascii_case(want))
                        .unwrap_or(false),
                    None => true,
                };
                if check && hash_ok {
                    s.set_state(ST_DONE);
                    // 整包路径不走 finalize，续传日志要自己清掉，否则下次会以为还缺块。
                    let _ = std::fs::remove_file(&s.journal);
                    on_job(s.orig);
                    // 进度线程已经收工了，这里手动补一次，免得大文件整包下载时进度不动。
                    on_tick(
                        done_bytes.load(Ordering::Relaxed),
                        total_known + extra_total.load(Ordering::Relaxed),
                    );
                } else {
                    let _ = std::fs::remove_file(&s.job.path);
                    s.fail(format!("{} 校验失败", s.job.path));
                }
            }
            Err(e) => s.fail(e),
        }
    }

    // 4) 大小未知的文件（没有 Range 可用）：整包下。
    for s in slots.iter() {
        if cancelled {
            break;
        }
        if s.job.size != 0 || s.state() != ST_PENDING {
            continue;
        }
        match crate::download_any(h, &s.job.urls, &s.job.path, s.job.size) {
            Ok(n) => {
                s.done.store(n, Ordering::Relaxed);
                done_bytes.fetch_add(n, Ordering::Relaxed);
                extra_total.fetch_add(n, Ordering::Relaxed);
                let hash_ok = match &s.job.sha1 {
                    Some(want) => sha1_file(Path::new(&s.job.path))
                        .map(|got| got.eq_ignore_ascii_case(want))
                        .unwrap_or(false),
                    None => true,
                };
                if hash_ok {
                    s.set_state(ST_DONE);
                    on_job(s.orig);
                    on_tick(
                        done_bytes.load(Ordering::Relaxed),
                        total_known + extra_total.load(Ordering::Relaxed),
                    );
                } else {
                    let _ = std::fs::remove_file(&s.job.path);
                    s.fail(format!("{} 校验失败", s.job.path));
                }
            }
            Err(e) => s.fail(e),
        }
    }

    // 5) 收尾：取消就清掉所有半成品；失败的文件也把 `.part` 删掉，别留垃圾。
    let mut done_count = skipped.len();
    for s in &slots {
        if cancelled {
            let _ = std::fs::remove_file(&s.part);
            continue;
        }
        match s.state() {
            ST_DONE => done_count += 1,
            ST_PENDING => {
                // 还有没领完的块（工作线程提前退出了，或者被限流放回了队列）：
                // 这里用一条连接把剩下的补完，别让用户拿到一个缺文件的版本。
                let left_ranges: Vec<(u64, u64)> = s
                    .pending
                    .lock()
                    .map(|q| q.iter().copied().collect())
                    .unwrap_or_default();
                let mut drained = true;
                for (off, len) in left_ranges {
                    let src = s.src.load(Ordering::Relaxed);
                    let url = crate::pick_url(&s.job.urls, src);
                    let req = json!({
                        "url": url,
                        "path": s.part.to_string_lossy(),
                        "offset": off,
                        "length": len,
                        "timeout_ms": 60_000u64
                    });
                    match call_json(cell.h(), "miy.net.download_range", &req) {
                        Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                            if let Ok(mut q) = s.pending.lock() {
                                q.retain(|&(o, l)| !(o == off && l == len));
                            }
                            s.done.fetch_add(len, Ordering::Relaxed);
                            done_bytes.fetch_add(len, Ordering::Relaxed);
                            append_journal(&s.journal, off, len);
                        }
                        _ => {
                            drained = false;
                            break;
                        }
                    }
                }
                if !drained {
                    s.fail(format!("{} 还有块没下完", s.job.path));
                    let _ = std::fs::remove_file(&s.part);
                    continue;
                }
                // 理论上不该出现（块都领完了），保险起见再校验一次。
                match finalize(s) {
                    Ok(()) => {
                        s.set_state(ST_DONE);
                        done_count += 1;
                        on_job(s.orig);
                    }
                    Err(msg) => s.fail(msg),
                }
            }
            _ => {
                let _ = std::fs::remove_file(&s.part);
            }
        }
        if let Ok(e) = s.err.lock() {
            if let Some(msg) = e.as_ref() {
                errors.push(msg.clone());
            }
        }
    }

    Report {
        bytes: done_bytes.load(Ordering::Relaxed),
        errors,
        cancelled,
        done: done_count,
    }
}

// ---- 回归测试 -------------------------------------------------------------
//
// 用一个假宿主把 `miy.net.download_range` / `miy.net.download` 换成「从本地
// 源文件按偏移读」，这样引擎的取消、重试、降级、校验、跳过都能在几秒内跑完，
// 不依赖网络，也不依赖真实宿主。

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

    struct TestCtx {
        src: PathBuf,
        calls: AtomicUsize,
        fail_first: AtomicUsize,
        norange: AtomicBool,
        cancel_after: AtomicUsize,
        cancel: AtomicUsize,
        /// 前 N 次 Range 请求只写一半字节（模拟服务器截断）。
        short_first: AtomicUsize,
        /// 前 N 次 Range 请求回 429（模拟镜像限流）。
        rl_first: AtomicUsize,
    }

    unsafe extern "C" fn fake_alloc(_ctx: *mut std::ffi::c_void, size: usize) -> *mut u8 {
        let layout = std::alloc::Layout::from_size_align(size.max(1), 8).unwrap();
        std::alloc::alloc(layout)
    }

    unsafe extern "C" fn fake_free(_ctx: *mut std::ffi::c_void, ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }
        let len = u32::from_le_bytes([*ptr, *ptr.add(1), *ptr.add(2), *ptr.add(3)]) as usize;
        let layout = std::alloc::Layout::from_size_align(4 + len, 8).unwrap();
        std::alloc::dealloc(ptr, layout);
    }

    unsafe fn put(s: &str) -> *mut u8 {
        let p = fake_alloc(std::ptr::null_mut(), 4 + s.len());
        (p as *mut u32).write_unaligned(s.len() as u32);
        std::ptr::copy_nonoverlapping(s.as_ptr(), p.add(4), s.len());
        p
    }

    unsafe extern "C" fn fake_call(
        ctx: *mut std::ffi::c_void,
        service: *const u8,
        service_len: usize,
        req: *const u8,
        req_len: usize,
    ) -> *mut u8 {
        let c = &*(ctx as *const TestCtx);
        let svc = std::str::from_utf8(std::slice::from_raw_parts(service, service_len)).unwrap_or("");
        let body = std::str::from_utf8(std::slice::from_raw_parts(req, req_len)).unwrap_or("");
        let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let n = c.calls.fetch_add(1, Ordering::SeqCst) + 1;

        // 到点就置取消，模拟用户点「取消」。
        let after = c.cancel_after.load(Ordering::SeqCst);
        if after > 0 && n >= after {
            let flag = c.cancel.load(Ordering::SeqCst) as *const AtomicBool;
            if !flag.is_null() {
                (*flag).store(true, Ordering::SeqCst);
            }
        }

        let resp = if svc == "miy.net.download_range" {
            if c.norange.load(Ordering::SeqCst) {
                json!({"ok": false, "kind": "norange", "status": 200, "error": "服务器不支持分段下载"})
            } else if n <= c.rl_first.load(Ordering::SeqCst) {
                json!({"ok": false, "kind": "http", "status": 429, "error": "HTTP 429"})
            } else if n <= c.fail_first.load(Ordering::SeqCst) {
                json!({"ok": false, "kind": "http", "status": 503, "error": "HTTP 503"})
            } else {
                let off = v.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
                let len = v.get("length").and_then(Value::as_u64).unwrap_or(0) as usize;
                let path = v.get("path").and_then(Value::as_str).unwrap_or("");
                let src = std::fs::read(&c.src).unwrap_or_default();
                let end = (off + len).min(src.len());
                let slice = &src[off.min(src.len())..end];
                // 模拟服务器截断：只写一半，并且如实报告写了多少。
                let short = n <= c.short_first.load(Ordering::SeqCst);
                let write = if short { &slice[..slice.len() / 2] } else { slice };
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(false)
                    .open(path)
                    .unwrap();
                use std::io::{Seek, SeekFrom, Write};
                f.seek(SeekFrom::Start(off as u64)).unwrap();
                f.write_all(write).unwrap();
                json!({"ok": true, "bytes": write.len()})
            }
        } else if svc == "miy.net.download" {
            let path = v.get("path").and_then(Value::as_str).unwrap_or("");
            let src = std::fs::read(&c.src).unwrap_or_default();
            std::fs::write(path, &src).unwrap();
            json!({"ok": true, "bytes": src.len()})
        } else {
            json!({"ok": false, "error": "unknown service"})
        };
        put(&resp.to_string())
    }

    fn host(ctx: &TestCtx) -> MiyHostV1 {
        MiyHostV1 {
            abi_version: miy_abi::MIY_ABI_VERSION,
            struct_size: std::mem::size_of::<MiyHostV1>() as u32,
            ctx: ctx as *const TestCtx as *mut std::ffi::c_void,
            log: None,
            alloc: Some(fake_alloc),
            free: Some(fake_free),
            call: Some(fake_call),
            task: None,
            emit: None,
        }
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("miy-dl-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ctx(src: &Path) -> TestCtx {
        TestCtx {
            src: src.to_path_buf(),
            calls: AtomicUsize::new(0),
            fail_first: AtomicUsize::new(0),
            norange: AtomicBool::new(false),
            cancel_after: AtomicUsize::new(0),
            cancel: AtomicUsize::new(0),
            short_first: AtomicUsize::new(0),
            rl_first: AtomicUsize::new(0),
        }
    }

    #[test]
    fn rate_limited_chunks_are_retried_not_failed() {
        let d = tmpdir("rl");
        let src = d.join("src.bin");
        let data: Vec<u8> = (0..(1 << 20)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        let c = ctx(&src);
        // 前两次请求被限流。以前这里直接 s.fail()，一个 429 就把整个文件判死，
        // 表现就是进度卡在 99%、装完缺文件；现在应该把块放回队列、等窗口重领。
        c.rl_first.store(2, Ordering::SeqCst);
        let h = host(&c);
        let out = d.join("out.bin");
        let jobs = vec![Job {
            // 用独立域名，免得和别的测试共用同一个限速闸门。
            urls: vec!["http://rl.test/x".into()],
            path: out.to_string_lossy().into(),
            size: data.len() as u64,
            sha1: None,
        }];
        let rep = run(&h, jobs, 4, None, &|_, _| {}, &|_| {});
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        assert_eq!(rep.done, 1);
        assert_eq!(std::fs::read(&out).unwrap(), data);
    }

    #[test]
    fn downloads_in_chunks_and_leaves_no_part() {
        let d = tmpdir("chunks");
        let src = d.join("src.bin");
        let data: Vec<u8> = (0..(1 << 20)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        let c = ctx(&src);
        let h = host(&c);
        let out = d.join("out.bin");
        let jobs = vec![Job {
            urls: vec!["u".into()],
            path: out.to_string_lossy().into(),
            size: data.len() as u64,
            sha1: None,
        }];
        let rep = run(&h, jobs, 4, None, &|_, _| {}, &|_| {});
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        assert_eq!(rep.done, 1);
        assert_eq!(std::fs::read(&out).unwrap(), data);
        assert!(!Path::new(&format!("{}.part", out.display())).exists());
    }

    #[test]
    fn hash_mismatch_leaves_nothing() {
        let d = tmpdir("hash");
        let src = d.join("src.bin");
        std::fs::write(&src, b"abc").unwrap();
        let c = ctx(&src);
        let h = host(&c);
        let out = d.join("out.bin");
        let jobs = vec![Job {
            urls: vec!["u".into()],
            path: out.to_string_lossy().into(),
            size: 3,
            sha1: Some("0000000000000000000000000000000000000000".into()),
        }];
        let rep = run(&h, jobs, 2, None, &|_, _| {}, &|_| {});
        assert!(!rep.errors.is_empty());
        assert_eq!(rep.done, 0);
        assert!(!out.exists());
        assert!(!Path::new(&format!("{}.part", out.display())).exists());
    }

    #[test]
    fn cancel_stops_and_cleans_up() {
        let d = tmpdir("cancel");
        let src = d.join("src.bin");
        let data = vec![7u8; 4 << 20];
        std::fs::write(&src, &data).unwrap();
        let mut c = ctx(&src);
        let flag = AtomicBool::new(false);
        c.cancel = AtomicUsize::new(&flag as *const AtomicBool as usize);
        c.cancel_after = AtomicUsize::new(2);
        let h = host(&c);
        let out = d.join("out.bin");
        let jobs = vec![Job {
            urls: vec!["u".into()],
            path: out.to_string_lossy().into(),
            size: data.len() as u64,
            sha1: None,
        }];
        let rep = run(&h, jobs, 4, Some(&flag), &|_, _| {}, &|_| {});
        assert!(rep.cancelled);
        assert!(!out.exists());
        assert!(!Path::new(&format!("{}.part", out.display())).exists());
    }

    #[test]
    fn norange_falls_back_to_whole_file() {
        let d = tmpdir("norange");
        let src = d.join("src.bin");
        let data = vec![3u8; 900 << 10];
        std::fs::write(&src, &data).unwrap();
        let mut c = ctx(&src);
        c.norange = AtomicBool::new(true);
        let h = host(&c);
        let out = d.join("out.bin");
        let jobs = vec![Job {
            urls: vec!["u".into()],
            path: out.to_string_lossy().into(),
            size: data.len() as u64,
            sha1: None,
        }];
        let rep = run(&h, jobs, 3, None, &|_, _| {}, &|_| {});
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        assert_eq!(std::fs::read(&out).unwrap(), data);
    }

    #[test]
    fn existing_file_is_skipped() {
        let d = tmpdir("skip");
        let src = d.join("src.bin");
        std::fs::write(&src, b"abc").unwrap();
        let c = ctx(&src);
        let h = host(&c);
        let out = d.join("out.bin");
        std::fs::write(&out, b"abc").unwrap();
        let jobs = vec![Job {
            urls: vec!["u".into()],
            path: out.to_string_lossy().into(),
            size: 3,
            sha1: Some("a9993e364706816aba3e25717850c26c9cd0d89d".into()),
        }];
        let rep = run(&h, jobs, 2, None, &|_, _| {}, &|_| {});
        assert!(rep.errors.is_empty());
        assert_eq!(rep.done, 1);
        assert_eq!(c.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn retries_after_server_error() {
        let d = tmpdir("retry");
        let src = d.join("src.bin");
        let data = vec![9u8; 300 << 10];
        std::fs::write(&src, &data).unwrap();
        let mut c = ctx(&src);
        c.fail_first = AtomicUsize::new(2);
        let h = host(&c);
        let out = d.join("out.bin");
        let jobs = vec![Job {
            urls: vec!["u".into()],
            path: out.to_string_lossy().into(),
            size: data.len() as u64,
            sha1: None,
        }];
        let rep = run(&h, jobs, 2, None, &|_, _| {}, &|_| {});
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        assert_eq!(std::fs::read(&out).unwrap(), data);
    }

    #[test]
    fn skip_then_download_reports_original_index() {
        let d = tmpdir("skipmix");
        let src = d.join("src.bin");
        let data = vec![5u8; 400 << 10];
        std::fs::write(&src, &data).unwrap();
        let c = ctx(&src);
        let h = host(&c);
        let first = d.join("a.bin");
        let second = d.join("b.bin");
        std::fs::write(&first, &data).unwrap(); // 已存在，会被跳过
        let jobs = vec![
            Job {
                urls: vec!["u".into()],
                path: first.to_string_lossy().into(),
                size: data.len() as u64,
                sha1: None,
            },
            Job {
                urls: vec!["u".into()],
                path: second.to_string_lossy().into(),
                size: data.len() as u64,
                sha1: None,
            },
        ];
        let seen = Mutex::new(Vec::new());
        let rep = run(&h, jobs, 2, None, &|_, _| {}, &|i| {
            seen.lock().unwrap().push(i)
        });
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        let mut got = seen.into_inner().unwrap();
        got.sort();
        assert_eq!(got, vec![0, 1], "回调必须给原始下标，跳过文件不能把后面的下标顶前");
    }

    #[test]
    fn short_range_write_is_retried() {
        let d = tmpdir("short");
        let src = d.join("src.bin");
        let data: Vec<u8> = (0..(600 << 10)).map(|i| (i % 97) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        let mut c = ctx(&src);
        c.short_first = AtomicUsize::new(1);
        let h = host(&c);
        let out = d.join("out.bin");
        let jobs = vec![Job {
            urls: vec!["u".into()],
            path: out.to_string_lossy().into(),
            size: data.len() as u64,
            sha1: None,
        }];
        let rep = run(&h, jobs, 2, None, &|_, _| {}, &|_| {});
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        assert_eq!(
            std::fs::read(&out).unwrap(),
            data,
            "短写必须重试，不能留下带洞的文件"
        );
    }

    #[test]
    fn norange_bytes_are_not_double_counted() {
        let d = tmpdir("norange-bytes");
        let src = d.join("src.bin");
        let data = vec![4u8; 700 << 10];
        std::fs::write(&src, &data).unwrap();
        let mut c = ctx(&src);
        c.norange = AtomicBool::new(true);
        let h = host(&c);
        let out = d.join("out.bin");
        let jobs = vec![Job {
            urls: vec!["u".into()],
            path: out.to_string_lossy().into(),
            size: data.len() as u64,
            sha1: None,
        }];
        let rep = run(&h, jobs, 3, None, &|_, _| {}, &|_| {});
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        assert_eq!(
            rep.bytes,
            data.len() as u64,
            "降级整包下载不能把已下块的字节重复计一遍"
        );
    }
}

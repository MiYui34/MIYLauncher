//! 插件总线：注册表、派发池、事件队列、宿主原语实现。
//!
//! 三条不变量（对着 `docs/architecture.md` §5，改代码前先读那三条）：
//!
//! 1. **同一个插件的 handler 永不并发执行。** 每个插件一把 [`Gate`]，派发前
//!    必须拿到它。插件作者因此可以放心用普通状态，不需要自己加锁。
//! 2. **每次跨边界调用都有 deadline。** 超时只是"放弃等待 + 记一笔"，绝不
//!    `TerminateThread` —— 被杀死的线程可能正持着 CRT 堆锁，那会把宿主一起带走。
//! 3. **handler 内部再 `call` 一律拒绝。** 用 thread_local 认，不靠插件自觉。
//!
//! 这三条之外还有一条网络时代的常识：**派发不跑在 UI 线程上**。UI 线程只负责
//! 画，任何一步卡住都是用户眼里的"未响应"。

use core::ffi::c_void;
use core::ptr;
use std::collections::{HashMap, VecDeque};
use std::ffi::CStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use miy_abi::*;
use windows_sys::Win32::Foundation::FreeLibrary;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
};

use crate::{heap, log, seh};

/// 默认调用超时。给得比较松：慢的是下载、解压，不是 ABI 本身。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// 派发池线程数。池子被"插件 A 等插件 B"的嵌套调用占满时会短暂饿死，
/// 超时兜底，所以不需要很大。
const POOL_SIZE: usize = 8;

// ---- 重入识别 --------------------------------------------------------------

thread_local! {
    /// 当前线程正在执行某个插件的 handler。
    static IN_HANDLER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn in_handler() -> bool {
    IN_HANDLER.with(|c| c.get())
}

// ---- 派发池 ----------------------------------------------------------------

type Job = Box<dyn FnOnce() + Send + 'static>;

/// 把裸指针裹成可以跨线程搬运的东西。
///
/// 任务体自己拥有 `arg` 的所有权（契约里写死：由任务体释放），所以"指针可以
/// 跨线程"这件事由 ABI 保证，这里只是替编译器承认。
pub struct SendArg(pub *mut c_void);
unsafe impl Send for SendArg {}

impl SendArg {
    fn take(self) -> *mut c_void {
        self.0
    }
}

static QUEUE: Mutex<VecDeque<Job>> = Mutex::new(VecDeque::new());
static QUEUE_CV: Condvar = Condvar::new();
static POOL_STARTED: OnceLock<()> = OnceLock::new();
/// 已经进入停机流程。加载线程靠它决定"还要不要往里塞"。
static STOPPING: AtomicBool = AtomicBool::new(false);

/// 是否正在停机（给加载线程与自检用）。
pub fn is_stopping() -> bool {
    STOPPING.load(Ordering::SeqCst)
}

fn pool_start() {
    POOL_STARTED.get_or_init(|| {
        for i in 0..POOL_SIZE {
            // 建不出来就少一个线程，不能让启动失败
            let _ = std::thread::Builder::new()
                .name(format!("miy-pool-{i}"))
                .spawn(|| loop {
                    let job = {
                        let mut q = match QUEUE.lock() {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
                        loop {
                            if let Some(job) = q.pop_front() {
                                break job;
                            }
                            q = match QUEUE_CV.wait(q) {
                                Ok(g) => g,
                                Err(p) => p.into_inner(),
                            };
                        }
                    };
                    job();
                });
        }
    });
}

fn submit_job(job: Job) {
    pool_start();
    if let Ok(mut q) = QUEUE.lock() {
        q.push_back(job);
    }
    QUEUE_CV.notify_one();
}

// ---- 每插件的互斥闸门 ------------------------------------------------------

struct Gate {
    busy: Mutex<bool>,
    cv: Condvar,
}

impl Gate {
    fn new() -> Self {
        Self {
            busy: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    /// 拿到闸门。`deadline` 为 `None` 表示无限等（只在退出流程里用）。
    fn acquire(&self, deadline: Option<Instant>) -> bool {
        let mut busy = match self.busy.lock() {
            Ok(b) => b,
            Err(p) => p.into_inner(),
        };
        loop {
            if !*busy {
                *busy = true;
                return true;
            }
            match deadline {
                None => {
                    busy = match self.cv.wait(busy) {
                        Ok(b) => b,
                        Err(p) => p.into_inner(),
                    };
                }
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return false;
                    }
                    let (b, timeout) = match self.cv.wait_timeout(busy, d - now) {
                        Ok(x) => x,
                        Err(p) => p.into_inner(),
                    };
                    busy = b;
                    if timeout.timed_out() && *busy {
                        return false;
                    }
                }
            }
        }
    }

    fn release(&self) {
        if let Ok(mut busy) = self.busy.lock() {
            *busy = false;
        }
        self.cv.notify_one();
    }
}

// ---- 插件句柄 --------------------------------------------------------------

struct Inner {
    state: *mut c_void,
    started: bool,
    faulted: bool,
    calls: u64,
    faults: u64,
    timeouts: u64,
}

pub struct PluginHandle {
    pub id: String,
    pub version: String,
    pub dir: PathBuf,
    pub dll: PathBuf,
    /// 这个插件贡献的 UI 页面（`contributes["ui.pages"]`）。
    pub pages: Vec<PageInfo>,
    /// 允许并发进入的服务全名（manifest 的 `concurrent`）。
    pub concurrent: Vec<String>,
    module: *mut c_void,
    stop_fn: Option<unsafe extern "C" fn(*mut c_void)>,
    dispatch_fn: Option<MiyDispatchFn>,
    inner: Mutex<Inner>,
    gate: Gate,
}

// `module` / `state` 是裸指针，但生命周期由总线独占管理：加载一次、退出时
// 释放；运行期只在拿到 `gate` 之后才读 `state`。
unsafe impl Send for PluginHandle {}
unsafe impl Sync for PluginHandle {}

impl PluginHandle {
    /// 这个服务是否被声明成"可以并发调用"。
    ///
    /// 只对插件自己在 manifest 里点名过的服务放行——宿主不替插件猜。
    fn is_concurrent(&self, service: &str) -> bool {
        // 精确匹配：`miy.mc.mod` 放行不该顺带把 `miy.mc.mod.install` 也放行
        self.concurrent.iter().any(|c| c == service)
    }

    /// 给 UI 看的快照。
    pub fn info(&self) -> PluginInfo {
        let i = match self.inner.lock() {
            Ok(i) => i,
            Err(p) => p.into_inner(),
        };
        PluginInfo {
            id: self.id.clone(),
            version: self.version.clone(),
            dir: self.dir.display().to_string(),
            pages: self.pages.clone(),
            started: i.started,
            faulted: i.faulted,
            calls: i.calls,
            faults: i.faults,
            timeouts: i.timeouts,
        }
    }
}

/// 插件贡献的一个 UI 页面。
///
/// 这是"一切皆插件"落到界面上的那一步：宿主只提供外壳和槽位，页面长什么样
/// 是插件自己的事（`plugins/<id>/ui/<entry>` 里的 HTML）。
#[derive(Clone)]
pub struct PageInfo {
    /// 贡献者。
    pub plugin: String,
    /// 页面 id，插件内唯一。
    pub id: String,
    pub title: String,
    /// `nav`（左侧/顶部导航）或 `home`（首页插槽）。
    pub slot: String,
    /// 相对 `plugins/<id>/ui/` 的 HTML 文件。
    pub entry: String,
    /// 导航里的排位（小的在前）。插件自己说了算，宿主不猜。
    pub order: i64,
}

/// 读 `contributes["ui.pages"]`。格式非法就当作没贡献，不让一个坏插件拖垮界面。
pub fn pages_of(plugin: &str, contributes: &serde_json::Value) -> Vec<PageInfo> {
    let Some(list) = contributes.get("ui.pages").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|p| {
            let id = p.get("id")?.as_str()?.trim().to_string();
            if id.is_empty() {
                return None;
            }
            let title = p
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string();
            let slot = p
                .get("slot")
                .and_then(|v| v.as_str())
                .unwrap_or("nav")
                .to_string();
            let entry = p
                .get("entry")
                .and_then(|v| v.as_str())
                .unwrap_or("page.html")
                .to_string();
            Some(PageInfo {
                plugin: plugin.to_string(),
                id,
                title,
                slot,
                entry,
                order: p.get("order").and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect()
}

#[derive(Clone)]
pub struct PluginInfo {
    pub id: String,
    pub version: String,
    /// 插件目录，给"打开文件夹"和排错用。
    pub dir: String,
    pub pages: Vec<PageInfo>,
    pub started: bool,
    pub faulted: bool,
    pub calls: u64,
    pub faults: u64,
    pub timeouts: u64,
}
// ---- 总线 ------------------------------------------------------------------

type HostService = Arc<dyn Fn(&[u8]) -> Vec<u8> + Send + Sync>;
type Waker = Arc<dyn Fn() + Send + Sync>;

pub struct Bus {
    plugins: RwLock<Vec<Arc<PluginHandle>>>,
    host_services: RwLock<HashMap<String, HostService>>,
    events: Mutex<VecDeque<(String, Vec<u8>)>>,
    tasks: Mutex<HashMap<u64, Arc<MiyCancel>>>,
    next_task: AtomicU64,
    waker: Mutex<Option<Waker>>,
}

static BUS: OnceLock<Bus> = OnceLock::new();

fn new_bus() -> Bus {
    Bus {
        plugins: RwLock::new(Vec::new()),
        host_services: RwLock::new(HashMap::new()),
        events: Mutex::new(VecDeque::new()),
        tasks: Mutex::new(HashMap::new()),
        next_task: AtomicU64::new(1),
        waker: Mutex::new(None),
    }
}

pub fn init() {
    let _ = BUS.set(new_bus());
    // 预热池子：第一次调用不该付线程创建的钱
    pool_start();
}

pub fn get() -> &'static Bus {
    BUS.get_or_init(new_bus)
}

impl Bus {
    // ---- 注册 ------------------------------------------------------------

    pub fn register_host_service<F>(&self, name: &str, f: F)
    where
        F: Fn(&[u8]) -> Vec<u8> + Send + Sync + 'static,
    {
        if let Ok(mut m) = self.host_services.write() {
            m.insert(name.to_string(), Arc::new(f));
        }
    }

    pub fn set_waker(&self, f: impl Fn() + Send + Sync + 'static) {
        if let Ok(mut w) = self.waker.lock() {
            *w = Some(Arc::new(f));
        }
    }

    pub fn insert(&self, h: Arc<PluginHandle>) {
        // 加载线程还在跑、主线程已经在停机：这个句柄不会再有人用，
        // 当场收掉。否则 DLL 留下变成"幽灵模块"，进程退出时才被回收。
        if STOPPING.load(Ordering::SeqCst) {
            log::warn(&format!("停机途中装载完成的插件被就地卸载: {}", h.id));
            teardown(&h);
            return;
        }
        // 内嵌插件先上桌，磁盘上残留的同 id 旧版本必须让位：两份 miy.mc
        // 抢同一个命名空间只会让路由结果取决于加载顺序。
        let dup = self
            .plugins
            .read()
            .map(|v| v.iter().any(|p| p.id == h.id))
            .unwrap_or(false);
        if dup {
            log::warn(&format!(
                "插件 {} 已经装过（内置优先），磁盘上这份被忽略: {}",
                h.id,
                h.dll.display()
            ));
            teardown(&h);
            return;
        }
        if let Ok(mut v) = self.plugins.write() {
            v.push(h);
        }
    }

    pub fn snapshot(&self) -> Vec<PluginInfo> {
        match self.plugins.read() {
            Ok(v) => v.iter().map(|p| p.info()).collect(),
            Err(p) => p.into_inner().iter().map(|p| p.info()).collect(),
        }
    }

    fn find(&self, service: &str) -> Option<Arc<PluginHandle>> {
        let plugins = match self.plugins.read() {
            Ok(v) => v,
            Err(p) => p.into_inner(),
        };
        // 路由规则：服务名必须以 `<plugin-id>.` 开头。插件只能注册自己的命名
        // 空间 —— 这是"谁也别想冒充别人"的第一道闸。
        plugins
            .iter()
            .find(|p| {
                service.len() > p.id.len()
                    && service.as_bytes().get(p.id.len()) == Some(&b'.')
                    && service.starts_with(&p.id)
            })
            .cloned()
    }

    // ---- 调用 ------------------------------------------------------------

    /// 同步调用 + 超时。UI 线程和插件都走这里。
    pub fn call(&self, service: &str, req: &[u8], timeout: Duration) -> Result<Vec<u8>, i32> {
        // 宿主服务是普通函数调用，不占派发池，**handler 里调它是安全的** ——
        // 版本解析、下载、登录全走这条路径。所以先查宿主服务，再判重入。
        if let Some(f) = self
            .host_services
            .read()
            .ok()
            .and_then(|m| m.get(service).cloned())
        {
            return Ok(f(req));
        }

        let Some(h) = self.find(service) else {
            return Err(MIY_ERR_NOT_FOUND);
        };

        if in_handler() {
            // §5：handler 内部再调**别的插件**一律拒绝。4 个池线程互等就是
            // 死锁，返回错误码让插件优雅降级。
            return Err(MIY_ERR_REENTRANT);
        }

        let (tx, rx) = mpsc::channel();
        let deadline = Instant::now() + timeout;
        let svc = service.to_string();
        let body = req.to_vec();
        submit_job(Box::new(move || {
            if Instant::now() >= deadline {
                // 已经没人等这个结果了：直接放弃，别让迟到的任务再占着插件
                let _ = tx.send(Err(MIY_ERR_TIMEOUT));
                return;
            }
            let _ = tx.send(dispatch_job(&h, &svc, &body, Some(deadline)));
        }));

        match rx.recv_timeout(timeout) {
            Ok(r) => r,
            Err(_) => {
                log::warn(&format!("调用 {service} 超时（{timeout:?}）"));
                Err(MIY_ERR_TIMEOUT)
            }
        }
    }

    pub fn call_default(&self, service: &str, req: &[u8]) -> Result<Vec<u8>, i32> {
        self.call(service, req, DEFAULT_TIMEOUT)
    }

    // ---- 事件 ------------------------------------------------------------

    /// `emit` 是入队，不是同步回调（§4）。真正的派发在主线程。
    pub fn emit(&self, topic: &str, payload: &[u8]) {
        if let Ok(mut q) = self.events.lock() {
            // 队列也要有上限：一个死循环 emit 的插件不该把内存吃光
            if q.len() >= 4096 {
                q.pop_front();
            }
            q.push_back((topic.to_string(), payload.to_vec()));
        }
        let w = self.waker.lock().ok().and_then(|w| w.clone());
        if let Some(w) = w {
            w();
        }
    }

    pub fn drain_events(&self, max: usize) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        if let Ok(mut q) = self.events.lock() {
            while out.len() < max {
                match q.pop_front() {
                    Some(e) => out.push(e),
                    None => break,
                }
            }
        }
        out
    }

    // ---- 任务 ------------------------------------------------------------

    /// 投递一个任务到池子，返回任务 id（0 表示失败）。
    ///
    /// 取消是**协作式**的：已经开始跑的任务不会被中断，取消只是把结果作废；
    /// 还没开始的任务则永远不会启动。任务体通过拿到的 `MiyCancel` 自己发现。
    pub fn spawn_task(&self, entry: MiyTaskEntry, arg: *mut c_void) -> u64 {
        let id = self.next_task.fetch_add(1, Ordering::Relaxed);
        let cancel = Arc::new(MiyCancel::new());
        if let Ok(mut m) = self.tasks.lock() {
            m.insert(id, cancel.clone());
        }

        let a = SendArg(arg);

        submit_job(Box::new(move || {
            // 整体消耗 `a`：否则 2021 的精确捕获会只抓 `a.0`（裸指针不 Send）。
            let arg = a.take();
            if cancel.is_cancelled() {
                log::debug(&format!("任务 {id} 在开始前被取消"));
                forget_task(id);
                return;
            }
            // 令牌活到任务结束：Arc 的这份引用保证指针不会悬空。
            let token = Arc::as_ptr(&cancel);
            IN_HANDLER.with(|c| c.set(true));
            let rc = seh::guard(|| unsafe { entry(arg, token) });
            IN_HANDLER.with(|c| c.set(false));
            if rc != MIY_OK {
                log::warn(&format!("任务 {id} 返回 {rc}"));
            }
            forget_task(id);
        }));
        id
    }

    pub fn cancel_task(&self, id: u64) -> bool {
        if let Ok(m) = self.tasks.lock() {
            if let Some(flag) = m.get(&id) {
                flag.cancel();
                return true;
            }
        }
        false
    }

    #[allow(dead_code)]
    pub fn live_tasks(&self) -> usize {
        self.tasks.lock().map(|m| m.len()).unwrap_or(0)
    }

    // ---- 退出 ------------------------------------------------------------

    /// 停插件。顺序与加载相反；闸门无限等是为了等手上那次调用收尾。
    pub fn shutdown_all(&self) {
        // 先立旗，让并发中的 load_all 别再往里塞新句柄
        STOPPING.store(true, Ordering::SeqCst);
        let plugins: Vec<Arc<PluginHandle>> = match self.plugins.write() {
            Ok(mut v) => v.drain(..).collect(),
            Err(p) => p.into_inner().drain(..).collect(),
        };
        for h in plugins.iter().rev() {
            teardown(h);
        }
    }
}

/// 收掉一个插件：等闸门 → stop → 吊销 ctx → 卸模块。
///
/// 三条都得按顺序来。先等闸门是因为 `stop` 可能正在跟某个 handler 抢同一份
/// 插件状态；吊销 ctx 在所有回调之后，防止插件在 `stop` 里偷偷再 `call`。
fn teardown(h: &PluginHandle) {
    let _ = h.gate.acquire(Some(Instant::now() + DEFAULT_TIMEOUT));
    let (stop_fn, state, started) = {
        let i = match h.inner.lock() {
            Ok(i) => i,
            Err(p) => p.into_inner(),
        };
        (h.stop_fn, i.state, i.started)
    };
    if started {
        if let Some(stop) = stop_fn {
            let _ = seh::guard(|| unsafe {
                stop(state);
                MIY_OK
            });
        }
    }
    // 吊销 ctx：此刻起这个插件的任何回调都是越界
    if let Ok(mut i) = h.inner.lock() {
        i.state = ptr::null_mut();
        i.started = false;
    }
    h.gate.release();
    // 内嵌插件是静态链接进来的，没有模块可卸。
    if !h.module.is_null() {
        unsafe { FreeLibrary(h.module) };
    }
    log::info(&format!("插件已卸载: {}", h.id));
}

fn forget_task(id: u64) {
    if let Some(bus) = BUS.get() {
        if let Ok(mut m) = bus.tasks.lock() {
            m.remove(&id);
        }
    }
}

// ---- 派发 ------------------------------------------------------------------

fn dispatch_job(
    h: &Arc<PluginHandle>,
    service: &str,
    req: &[u8],
    deadline: Option<Instant>,
) -> Result<Vec<u8>, i32> {
    // 默认一个插件同时只跑一个 handler。但联网这类慢活会把它后面所有调用
    // 一起堵住（典型症状：模组页在搜、切到游戏页就空白几十秒）。
    // 插件可以在 manifest 里把**无共享状态**的服务列进 `concurrent`，
    // 那些服务的调用就不再排闸门。
    let gated = !h.is_concurrent(service);
    if gated && !h.gate.acquire(deadline) {
        if let Ok(mut i) = h.inner.lock() {
            i.timeouts += 1;
        }
        return Err(MIY_ERR_TIMEOUT);
    }

    let out = dispatch_locked(h, service, req);
    if gated {
        h.gate.release();
    }

    if out.as_ref().err() == Some(&MIY_ERR_FAULT) {
        // 熔断：这个插件已经证明自己会炸，从此刻起不再接客
        if let Ok(mut i) = h.inner.lock() {
            i.faulted = true;
            i.state = ptr::null_mut();
            i.faults += 1;
        }
        log::error(&format!("插件 {} 触发硬件异常，已熔断", h.id));
    }

    out
}

fn dispatch_locked(h: &Arc<PluginHandle>, service: &str, req: &[u8]) -> Result<Vec<u8>, i32> {
    let (state, faulted) = {
        let i = match h.inner.lock() {
            Ok(i) => i,
            Err(p) => p.into_inner(),
        };
        (i.state, i.faulted)
    };

    if faulted {
        return Err(MIY_ERR_FAULT);
    }
    let Some(df) = h.dispatch_fn else {
        return Err(MIY_ERR_UNKNOWN_SERVICE);
    };

    let mut out: *mut u8 = ptr::null_mut();
    let svc = service.as_bytes();
    let req_ptr = if req.is_empty() {
        ptr::null()
    } else {
        req.as_ptr()
    };

    // 跨边界：SEH 挡硬件异常，panic 由插件自己在边界内 `catch_unwind`
    IN_HANDLER.with(|c| c.set(true));
    let rc = seh::guard(|| unsafe {
        df(state, svc.as_ptr(), svc.len(), req_ptr, req.len(), &mut out)
    });
    IN_HANDLER.with(|c| c.set(false));

    if let Ok(mut i) = h.inner.lock() {
        i.calls += 1;
    }

    if rc != MIY_OK {
        return Err(rc);
    }
    if out.is_null() {
        // 返回成功却给空指针，是契约违约
        return Err(MIY_ERR_INVALID_ARG);
    }
    let bytes = unsafe { read_buf(out).to_vec() };
    heap::free_buf(out);
    Ok(bytes)
}

// ---- 加载 ------------------------------------------------------------------

pub struct Candidate {
    pub dir: PathBuf,
    pub manifest: PathBuf,
}

pub fn discover(root: &Path) -> Vec<Candidate> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();

    for p in entries {
        if p.is_dir() {
            let m = p.join("manifest.json");
            if m.is_file() {
                out.push(Candidate {
                    dir: p,
                    manifest: m,
                });
            }
        } else if p
            .extension()
            .map(|e| e.eq_ignore_ascii_case("dll"))
            .unwrap_or(false)
        {
            let m = p.with_extension("manifest.json");
            if m.is_file() {
                out.push(Candidate {
                    dir: p.parent().unwrap_or(root).to_path_buf(),
                    manifest: m,
                });
            }
        }
    }
    out
}

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// manifest 的通用校验：id 合法、ABI 对得上。
fn check_manifest(m: &Manifest) -> Result<(), String> {
    if m.id.is_empty() {
        return Err("manifest.id 为空".into());
    }
    if !m
        .id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
    {
        return Err(format!("manifest.id 含非法字符: {}", m.id));
    }
    if m.abi != MIY_ABI_VERSION {
        return Err(format!(
            "ABI 不匹配：插件要 v{}，宿主是 v{}",
            m.abi, MIY_ABI_VERSION
        ));
    }
    Ok(())
}

/// 把一份已经拿到的入口表装成插件：校验 + `start` + 建句柄。
///
/// 外置 DLL 与内嵌插件共用这一段，区别只有 `module`：内嵌是空指针，
/// `stop` 之后不需要 `FreeLibrary`。
fn adopt(
    manifest: &Manifest,
    dir: PathBuf,
    dll: PathBuf,
    module: *mut c_void,
    api: *const MiyPluginV1,
) -> Result<PluginHandle, String> {
    let sz = unsafe { ptr::addr_of!((*api).struct_size).read_unaligned() } as usize;
    let abi = unsafe { ptr::addr_of!((*api).abi_version).read_unaligned() };
    let base = core::mem::offset_of!(MiyPluginV1, dispatch);
    if abi != MIY_ABI_VERSION {
        return Err(format!(
            "入口声明的 ABI v{abi} 与宿主 v{MIY_ABI_VERSION} 不符"
        ));
    }
    if sz < base {
        return Err(format!("struct_size {sz} 小于必需值 {base}"));
    }

    let id_ptr = unsafe { ptr::addr_of!((*api).id).read_unaligned() };
    if id_ptr.is_null() {
        return Err("id 为空".into());
    }
    let id = unsafe { CStr::from_ptr(id_ptr) }
        .to_string_lossy()
        .into_owned();
    if id != manifest.id {
        return Err(format!(
            "id 不一致：manifest 是 {}，入口声明 {id}",
            manifest.id
        ));
    }
    let ver_ptr = unsafe { ptr::addr_of!((*api).version).read_unaligned() };
    let version = if ver_ptr.is_null() {
        manifest.version.clone()
    } else {
        unsafe { CStr::from_ptr(ver_ptr) }
            .to_string_lossy()
            .into_owned()
    };

    let start = unsafe { ptr::addr_of!((*api).start).read_unaligned() };
    let stop_fn = unsafe { ptr::addr_of!((*api).stop).read_unaligned() };
    let dispatch_fn = if sz >= base + core::mem::size_of::<Option<MiyDispatchFn>>() {
        unsafe { ptr::addr_of!((*api).dispatch).read_unaligned() }
    } else {
        None
    };

    let Some(start) = start else {
        return Err("start 为空".into());
    };

    let mut state: *mut c_void = ptr::null_mut();
    let host = host_api();
    let rc = seh::guard(|| unsafe { start(&host, &mut state) });
    if rc != MIY_OK {
        return Err(format!("start 返回 {rc}"));
    }

    let pages = pages_of(&id, &manifest.contributes);
    // manifest 里点名允许并发的服务（见 dispatch_job 的注释）
    let concurrent: Vec<String> = manifest
        .concurrent
        .iter()
        .map(|s| format!("{id}.{s}"))
        .collect();

    Ok(PluginHandle {
        id,
        version,
        dir,
        dll,
        pages,
        concurrent,
        module,
        stop_fn,
        dispatch_fn,
        inner: Mutex::new(Inner {
            state,
            started: true,
            faulted: false,
            calls: 0,
            faults: 0,
            timeouts: 0,
        }),
        gate: Gate::new(),
    })
}

/// 内嵌插件：manifest 与入口表都在 exe 里，不读磁盘、不 `LoadLibrary`。
///
/// 这条路让「下载一个 exe 就能启动游戏」成立；磁盘上如果还留着同 id 的
/// 旧插件，[`Bus::insert`] 会把后来者拒掉，不会出现两份 miy.mc 抢命名空间。
pub fn load_embedded(manifest_json: &str, entry: MiyEntryFn) -> Result<PluginHandle, String> {
    let manifest = Manifest::from_slice(manifest_json.as_bytes())
        .map_err(|e| format!("内嵌 manifest 不是合法 JSON: {e}"))?;
    check_manifest(&manifest)?;
    crate::settings::contribute(&manifest.id, &manifest.contributes);

    let mut api: *const MiyPluginV1 = ptr::null();
    let rc = seh::guard(|| {
        api = unsafe { entry() };
        MIY_OK
    });
    if rc != MIY_OK {
        return Err(format!("内嵌入口崩溃（rc={rc}）"));
    }
    if api.is_null() {
        return Err("内嵌入口返回空指针".into());
    }
    adopt(
        &manifest,
        PathBuf::new(),
        PathBuf::from("<内嵌>"),
        ptr::null_mut(),
        api,
    )
}

/// 加载一个插件并 `start` 它。返回的句柄还没进注册表。
pub fn load_one(c: &Candidate) -> Result<PluginHandle, String> {
    let bytes = std::fs::read(&c.manifest).map_err(|e| format!("读 manifest 失败: {e}"))?;
    let manifest =
        Manifest::from_slice(&bytes).map_err(|e| format!("manifest 不是合法 JSON: {e}"))?;

    check_manifest(&manifest)?;

    // 贡献点先并入宿主注册表：设置页不该等插件句柄建好才出现它的项。
    crate::settings::contribute(&manifest.id, &manifest.contributes);

    let dll_name = manifest        .entry
        .clone()
        .unwrap_or_else(|| format!("{}.dll", manifest.id));
    let dll = c.dir.join(&dll_name);
    if !dll.is_file() {
        return Err(format!("找不到 dll: {}", dll.display()));
    }

    let module = unsafe {
        LoadLibraryExW(
            wide(dll.as_os_str()).as_ptr(),
            ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        )
    };
    if module.is_null() {
        return Err(format!(
            "LoadLibraryExW 失败（错误码 {}）: {}",
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
            dll.display()
        ));
    }

    let sym = unsafe { GetProcAddress(module, MIY_ENTRY_SYMBOL.as_ptr()) };
    let Some(sym) = sym else {
        unsafe { FreeLibrary(module) };
        return Err("缺少导出符号 miy_plugin_entry_v1".into());
    };
    let entry: MiyEntryFn = unsafe { core::mem::transmute(sym) };

    let mut entry_out: isize = 0;
    let rc = seh::guard(|| {
        entry_out = unsafe { entry() } as isize;
        MIY_OK
    });
    if rc != MIY_OK {
        unsafe { FreeLibrary(module) };
        return Err(format!("调用入口时崩溃（rc={rc}）"));
    }

    let api = entry_out as *const MiyPluginV1;
    if api.is_null() {
        unsafe { FreeLibrary(module) };
        return Err("入口返回空指针".into());
    }

    match adopt(&manifest, c.dir.clone(), dll, module, api) {
        Ok(h) => Ok(h),
        Err(e) => {
            unsafe { FreeLibrary(module) };
            Err(e)
        }
    }
}

/// 发现 + 加载全部插件。**在后台线程调用**：冷启动时单个插件 110–130 ms，
/// 全放在 UI 线程上就是肉眼可见的卡顿。
/// 只读 manifest 里的 id —— 在真正 LoadLibrary 之前判断这个 id 是否已在册。
fn peek_id(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("manifest.json")).ok()?;
    let m: Manifest = serde_json::from_str(&text).ok()?;
    Some(m.id)
}

pub fn load_all(root: &Path) -> Vec<Result<String, String>> {
    let bus = get();
    let mut out = Vec::new();
    for c in discover(root) {
        // 已经有人在关窗了，就别再往进程里塞 DLL
        if is_stopping() {
            log::warn("停机途中放弃继续加载插件");
            break;
        }
        // 内置插件（编进 exe 的 miy.mc）已经占了这个 id：磁盘上那份老版本
        // 不再 LoadLibrary。不然它的设置项会参与合并、start 完再被拆掉，
        // 用户升级后留下的 plugins\miy.mc 就成了纯噪声。
        if let Some(id) = peek_id(&c.dir) {
            if bus.snapshot().iter().any(|p| p.id == id) {
                let msg = format!("{id} 已内置在 exe 里，磁盘上的旧版本被跳过");
                log::info(&format!("[plugin] {msg}（{}）", c.dir.display()));
                bus.emit("plugin.skipped", msg.as_bytes());
                out.push(Ok(msg));
                continue;
            }
        }
        let t = Instant::now();
        match load_one(&c) {
            Ok(h) => {
                let summary = format!(
                    "{} v{} 加载成功（{} µs）",
                    h.id,
                    h.version,
                    t.elapsed().as_micros()
                );
                log::info(&format!("[plugin] {summary}"));
                bus.insert(Arc::new(h));
                bus.emit("plugin.loaded", summary.as_bytes());
                out.push(Ok(summary));
            }
            Err(e) => {
                let msg = format!("{}: {e}", c.dir.display());
                log::error(&format!("[plugin] {msg}"));
                bus.emit("plugin.failed", msg.as_bytes());
                out.push(Err(msg));
            }
        }
    }
    bus.emit("plugin.ready", b"");
    out
}

// ---- 宿主函数表（插件看到的 MiyHostV1）-------------------------------------

#[repr(transparent)]
struct HostApiCell(MiyHostV1);
unsafe impl Sync for HostApiCell {}
unsafe impl Send for HostApiCell {}

static HOST_API: OnceLock<HostApiCell> = OnceLock::new();

fn make_host_api() -> HostApiCell {
    HostApiCell(MiyHostV1 {
        abi_version: MIY_ABI_VERSION,
        struct_size: core::mem::size_of::<MiyHostV1>() as u32,
        ctx: ptr::null_mut(),
        log: Some(host_log),
        alloc: Some(host_alloc),
        free: Some(host_free),
        call: Some(host_call),
        task: Some(host_task),
        emit: Some(host_emit),
    })
}

pub fn init_host_api() {
    HOST_API.get_or_init(make_host_api);
}

fn host_api() -> MiyHostV1 {
    HOST_API.get_or_init(make_host_api).0
}

unsafe extern "C" fn host_log(_ctx: *mut c_void, level: i32, msg: *const u8, len: usize) {
    let text = if msg.is_null() {
        ""
    } else {
        std::str::from_utf8(core::slice::from_raw_parts(msg, len)).unwrap_or("<非法 UTF-8>")
    };
    match level {
        MIY_LOG_ERROR => log::error(text),
        MIY_LOG_WARN => log::warn(text),
        MIY_LOG_DEBUG | MIY_LOG_TRACE => log::debug(text),
        _ => log::info(text),
    }
}

unsafe extern "C" fn host_alloc(_ctx: *mut c_void, size: usize) -> *mut u8 {
    heap::alloc_bytes(size)
}

unsafe extern "C" fn host_free(_ctx: *mut c_void, p: *mut u8) {
    heap::free_buf(p)
}

unsafe extern "C" fn host_call(
    _ctx: *mut c_void,
    service: *const u8,
    service_len: usize,
    req: *const u8,
    req_len: usize,
) -> *mut u8 {
    let name = if service.is_null() {
        ""
    } else {
        std::str::from_utf8(core::slice::from_raw_parts(service, service_len)).unwrap_or("")
    };
    let body: &[u8] = if req.is_null() || req_len == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(req, req_len)
    };

    match get().call(name, body, DEFAULT_TIMEOUT) {
        Ok(v) => heap::alloc_buf(&v),
        Err(code) => {
            // 契约（architecture.md §5）规定这些失败要带 human-readable 的 code，
            // 不要只丢一个数字给插件去猜。
            let body = match code {
                MIY_ERR_REENTRANT => br#"{"code":"reentrant"}"#.to_vec(),
                MIY_ERR_TIMEOUT => br#"{"code":"timeout"}"#.to_vec(),
                MIY_ERR_NOT_FOUND => br#"{"code":"not-found"}"#.to_vec(),
                other => format!(r#"{{"code":"error","err":{other}}}"#).into_bytes(),
            };
            heap::alloc_buf(&body)
        }
    }
}

unsafe extern "C" fn host_task(
    _ctx: *mut c_void,
    entry: Option<MiyTaskEntry>,
    arg: *mut c_void,
) -> u64 {
    match entry {
        Some(e) => get().spawn_task(e, arg),
        None => 0,
    }
}

unsafe extern "C" fn host_emit(
    _ctx: *mut c_void,
    topic: *const u8,
    topic_len: usize,
    payload: *const u8,
    payload_len: usize,
) {
    if topic.is_null() {
        return;
    }
    let t = std::str::from_utf8(core::slice::from_raw_parts(topic, topic_len)).unwrap_or("?");
    let p: &[u8] = if payload.is_null() || payload_len == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(payload, payload_len)
    };
    get().emit(t, p);
}

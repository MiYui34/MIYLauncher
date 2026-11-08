//! MIYLauncher 插件 ABI v1（修订 1.1）。
//!
//! 这是宿主与插件之间**唯一**的契约，改这里等于改全世界的插件。
//! 规则见 `docs/architecture.md` §4：
//!
//! 1. 所有字符串都是 UTF-8 + 显式长度，永不以 NUL 结尾作为契约；
//! 2. 跨边界的堆内存只能用宿主给的 `alloc` / `free`；
//! 3. `stop` 返回后 `ctx` 立即吊销，再调用就是未定义行为；
//! 4. `emit` 是入队，不是同步回调；
//! 5. 任务取消是协作式的，宿主永不 `TerminateThread`；
//! 6. panic 不得跨边界 —— 插件必须在自己的 `extern "C"` 函数**内部**
//!    catch_unwind，宿主在边界外面用 SEH 兜访问冲突。
//!
//! 结构体一律"只追加"：新字段加在尾部，旧插件靠 `struct_size` 自证长度。

use core::ffi::{c_char, c_void};

/// 当前 ABI 主版本。不兼容的改动必须 +1。
pub const MIY_ABI_VERSION: u32 = 1;
/// ABI 修订号，纯信息性，不参与兼容判断。
///
/// 2：`MiyTaskEntry` 增加 `cancel` 参数（协作式取消的令牌）。
pub const MIY_ABI_REVISION: u32 = 2;

/// 插件入口导出符号（C 字符串，含 NUL）。
pub const MIY_ENTRY_SYMBOL: &[u8] = b"miy_plugin_entry_v1\0";

// ---- 状态码 ---------------------------------------------------------------

pub const MIY_OK: i32 = 0;
pub const MIY_ERR_INVALID_ARG: i32 = -1;
pub const MIY_ERR_ABI: i32 = -2;
pub const MIY_ERR_UNKNOWN_SERVICE: i32 = -3;
pub const MIY_ERR_PANIC: i32 = -4;
pub const MIY_ERR_REENTRANT: i32 = -5;
pub const MIY_ERR_TIMEOUT: i32 = -6;
pub const MIY_ERR_NOT_FOUND: i32 = -7;
/// 宿主侧 SEH 捕获到访问冲突/非法指令等硬件异常。
pub const MIY_ERR_FAULT: i32 = -8;

/// `miy_guard` 在捕获到硬件异常时返回的哨兵值。
pub const MIY_FAULT_SENTINEL: i32 = 0x7A11;

// ---- 日志等级 -------------------------------------------------------------

pub const MIY_LOG_TRACE: i32 = 0;
pub const MIY_LOG_DEBUG: i32 = 1;
pub const MIY_LOG_INFO: i32 = 2;
pub const MIY_LOG_WARN: i32 = 3;
pub const MIY_LOG_ERROR: i32 = 4;

// ---- 缓冲区约定 -----------------------------------------------------------
//
// 所有跨边界的堆缓冲区都是 `[u32 LE 载荷长度][载荷字节]`。
// 长度前缀让 `free` 不需要额外参数就能还原布局，这是"只用宿主分配器"
// 这条规则能成立的前提。

/// 缓冲区头长度。
pub const MIY_BUF_HDR: usize = 4;
/// 单个缓冲区载荷上限（防御恶意/失控插件）。
pub const MIY_BUF_MAX: usize = 16 * 1024 * 1024;

/// 写入长度前缀 + 载荷，调用方负责分配 `payload.len() + MIY_BUF_HDR` 字节。
///
/// # Safety
/// `dst` 必须指向至少 `payload.len() + MIY_BUF_HDR` 字节的可写内存。
pub unsafe fn write_buf(dst: *mut u8, payload: &[u8]) {
    let n = payload.len() as u32;
    core::ptr::copy_nonoverlapping(n.to_le_bytes().as_ptr(), dst, MIY_BUF_HDR);
    core::ptr::copy_nonoverlapping(payload.as_ptr(), dst.add(MIY_BUF_HDR), payload.len());
}

/// 读取长度前缀后的载荷。
///
/// # Safety
/// `p` 必须是本 ABI 的 `alloc` 返回的有效缓冲区。
pub unsafe fn read_buf<'a>(p: *const u8) -> &'a [u8] {
    let mut hdr = [0u8; MIY_BUF_HDR];
    core::ptr::copy_nonoverlapping(p, hdr.as_mut_ptr(), MIY_BUF_HDR);
    let n = u32::from_le_bytes(hdr) as usize;
    core::slice::from_raw_parts(p.add(MIY_BUF_HDR), n)
}

/// 由缓冲区指针反推它占用的总字节数（供 `free` 还原 `Vec` 布局）。
///
/// # Safety
/// 同 [`read_buf`]。
pub unsafe fn buf_total_len(p: *const u8) -> usize {
    let mut hdr = [0u8; MIY_BUF_HDR];
    core::ptr::copy_nonoverlapping(p, hdr.as_mut_ptr(), MIY_BUF_HDR);
    u32::from_le_bytes(hdr) as usize + MIY_BUF_HDR
}

// ---- 函数指针类型 ---------------------------------------------------------

pub type MiyLogFn =
    unsafe extern "C" fn(ctx: *mut c_void, level: i32, msg: *const u8, msg_len: usize);

pub type MiyAllocFn = unsafe extern "C" fn(ctx: *mut c_void, size: usize) -> *mut u8;

pub type MiyFreeFn = unsafe extern "C" fn(ctx: *mut c_void, ptr: *mut u8);

/// 调用一个 service，返回 `[len][json]` 缓冲区；失败返回 null。
pub type MiyCallFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    service: *const u8,
    service_len: usize,
    req: *const u8,
    req_len: usize,
) -> *mut u8;

/// 协作式取消令牌。
///
/// 宿主把它的指针交给任务体，任务体按自己的节奏 `is_cancelled()` 一下。
/// **永远不会有线程被强杀**：取消只是把结果作废，让任务体尽快自己退出。
///
/// 用 `AtomicU32` 而不是布尔：C / C++ / Zig 侧都能当成一个 32 位整数直接读，
/// 不需要理解 Rust 的 `bool` 布局。
#[repr(C)]
pub struct MiyCancel {
    flag: std::sync::atomic::AtomicU32,
}

impl MiyCancel {
    pub const fn new() -> Self {
        Self {
            flag: std::sync::atomic::AtomicU32::new(0),
        }
    }
    /// 宿主侧调用：请求取消。
    pub fn cancel(&self) {
        self.flag.store(1, std::sync::atomic::Ordering::Release);
    }
    /// 任一侧调用：是否已被取消。
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::Acquire) != 0
    }
}

impl Default for MiyCancel {
    fn default() -> Self {
        Self::new()
    }
}

/// 任务体。`cancel` 指向宿主持有的取消令牌，随任务存活。
pub type MiyTaskEntry = unsafe extern "C" fn(arg: *mut c_void, cancel: *const MiyCancel) -> i32;

/// 投递一个任务，返回任务 id（0 = 失败）。取消通过 id + 协作式标志完成。
pub type MiyTaskFn =
    unsafe extern "C" fn(ctx: *mut c_void, entry: Option<MiyTaskEntry>, arg: *mut c_void) -> u64;

/// 发事件：异步入队，宿主在主线程派发。
pub type MiyEmitFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    topic: *const u8,
    topic_len: usize,
    payload: *const u8,
    payload_len: usize,
);

// ---- 宿主 → 插件 ----------------------------------------------------------

/// 宿主提供给插件的 v1 能力表。
///
/// 插件收到的是**只读副本**；`ctx` 只对本次会话有效，`stop` 之后失效。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MiyHostV1 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub ctx: *mut c_void,
    pub log: Option<MiyLogFn>,
    pub alloc: Option<MiyAllocFn>,
    pub free: Option<MiyFreeFn>,
    pub call: Option<MiyCallFn>,
    pub task: Option<MiyTaskFn>,
    pub emit: Option<MiyEmitFn>,
}

impl MiyHostV1 {
    /// 用宿主分配器分配一块带长度前缀的缓冲区并写入载荷。
    ///
    /// # Safety
    /// 必须由本结构体的 `alloc` 分配、由同一个 `free` 释放。
    pub unsafe fn alloc_buf(&self, payload: &[u8]) -> *mut u8 {
        let Some(alloc) = self.alloc else { return core::ptr::null_mut() };
        let total = payload.len() + MIY_BUF_HDR;
        let p = alloc(self.ctx, total);
        if p.is_null() {
            return core::ptr::null_mut();
        }
        write_buf(p, payload);
        p
    }

    /// # Safety
    /// `p` 必须是本结构体 `alloc` 出来的缓冲区或 null。
    pub unsafe fn free_buf(&self, p: *mut u8) {
        if p.is_null() {
            return;
        }
        if let Some(free) = self.free {
            free(self.ctx, p);
        }
    }

    /// # Safety
    /// 调用任意宿主函数，需保证宿主仍然存活。
    pub unsafe fn log_bytes(&self, level: i32, msg: &[u8]) {
        if let Some(log) = self.log {
            log(self.ctx, level, msg.as_ptr(), msg.len());
        }
    }

    /// # Safety
    /// 同 [`Self::log_bytes`]。
    pub unsafe fn log_str(&self, level: i32, msg: &str) {
        self.log_bytes(level, msg.as_bytes());
    }

    /// 同步调用一个 service，返回 JSON 载荷。
    ///
    /// # Safety
    /// 同 [`Self::log_bytes`]。返回的 `Vec` 是拷贝，宿主缓冲区已释放。
    pub unsafe fn call_json(&self, service: &str, req: &[u8]) -> Option<Vec<u8>> {
        let call = self.call?;
        let p = call(
            self.ctx,
            service.as_ptr(),
            service.len(),
            req.as_ptr(),
            req.len(),
        );
        if p.is_null() {
            return None;
        }
        let out = read_buf(p).to_vec();
        self.free_buf(p);
        Some(out)
    }

    /// 发一个事件给宿主。异步入队，宿主在主线程派发，这里不会阻塞。
    ///
    /// # Safety
    /// 同 [`Self::log_bytes`]。
    pub unsafe fn emit_str(&self, topic: &str, payload: &[u8]) {
        if let Some(emit) = self.emit {
            emit(
                self.ctx,
                topic.as_ptr(),
                topic.len(),
                payload.as_ptr(),
                payload.len(),
            );
        }
    }

    /// 投一个后台任务，返回任务 id（0 表示宿主没接）。取消是协作式的：
    /// 任务体要定期看 `cancel_flag`。
    ///
    /// # Safety
    /// `arg` 的所有权交给任务体，必须由任务体自己释放。
    pub unsafe fn spawn(&self, entry: MiyTaskEntry, arg: *mut c_void) -> u64 {
        match self.task {
            Some(task) => task(self.ctx, Some(entry), arg),
            None => 0,
        }
    }
}

// ---- 插件 → 宿主 ----------------------------------------------------------

/// 插件把 `call("<plugin-id>.*")` 落到自己身上的入口（v1.1 追加）。
///
/// * `req` 是请求 JSON（`req_len` 可为 0）
/// * `out` 写入 `[len][json]` 缓冲区，**必须**用宿主 `alloc` 分配，宿主负责
///   用同一个 `free` 释放
/// * 返回 [`MIY_OK`] 或负的错误码
pub type MiyDispatchFn = unsafe extern "C" fn(
    state: *mut c_void,
    service: *const u8,
    service_len: usize,
    req: *const u8,
    req_len: usize,
    out: *mut *mut u8,
) -> i32;

/// 插件元数据 + 生命周期。`struct_size` 之后的字段是可选的。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MiyPluginV1 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub id: *const c_char,
    pub version: *const c_char,
    pub start: Option<unsafe extern "C" fn(host: *const MiyHostV1, out_state: *mut *mut c_void) -> i32>,
    pub stop: Option<unsafe extern "C" fn(state: *mut c_void)>,
    /// v1.1 追加字段。
    pub dispatch: Option<MiyDispatchFn>,
}

/// 插件实现这个符号并导出：`extern "C" fn miy_plugin_entry_v1() -> *const MiyPluginV1`。
pub type MiyEntryFn = unsafe extern "C" fn() -> *const MiyPluginV1;

/// 把 [`MiyPluginV1`] 装进 `static` 用的外壳。
///
/// `MiyPluginV1` 里有 `*const c_char`，所以它不是 `Sync`，没法直接当
/// `static`。但那张表在插件里自始至终只读、且编译期就固定，跨线程共享是
/// 安全的 —— 这个 `unsafe impl` 就是在替编译器承认这一点。
#[repr(transparent)]
pub struct PluginEntryCell(pub MiyPluginV1);

unsafe impl Sync for PluginEntryCell {}

/// 宿主结构体里某个字段是否可用（旧插件读新宿主、旧宿主读新插件都靠它）。
#[inline]
pub fn field_available(struct_size: u32, offset: usize, size: usize) -> bool {
    (struct_size as usize) >= offset + size
}

/// 插件侧把内部实现包进 `catch_unwind`，保证 panic 不越过 FFI。
///
/// 只在 `panic = "unwind"` 的插件构建里真正起作用（见工作区
/// `[profile.plugin]`）。
pub fn catch_panic<T>(f: impl FnOnce() -> T, fallback: T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => fallback,
    }
}

// ---- manifest -------------------------------------------------------------

#[cfg(feature = "manifest")]
mod manifest_impl {
    use serde::Deserialize;

    fn default_abi() -> u32 {
        super::MIY_ABI_VERSION
    }

    fn default_true() -> bool {
        true
    }

    /// `plugins/<name>/manifest.json`。
    #[derive(Debug, Clone, Deserialize)]
    pub struct Manifest {
        /// 反向域名 id，同时是 service 前缀：`miy.hello.*`
        pub id: String,
        #[serde(default)]
        pub name: String,
        #[serde(default)]
        pub version: String,
        #[serde(default)]
        pub description: String,
        #[serde(default)]
        pub author: String,
        /// 期望的 ABI 主版本。
        #[serde(default = "default_abi")]
        pub abi: u32,
        /// 相对 manifest 所在目录的 dll 路径；缺省为 `<目录名>.dll`。
        #[serde(default)]
        pub entry: Option<String>,
        /// 由宿主激活还是等用户点开。
        #[serde(default = "default_true")]
        pub enabled: bool,
        /// 加载顺序，越大越先。
        #[serde(default)]
        pub priority: i64,
        /// 是否随宿主启动即加载（否则懒加载）。
        #[serde(default = "default_true")]
        pub eager: bool,
        /// 声明的贡献点（UI 入口、hud、hook 等），v1 只是原样交给前端。
        #[serde(default)]
        pub contributes: serde_json::Value,
        /// 允许并发进入的服务名（不含插件前缀），例如 `["mods", "versions"]`。
        ///
        /// 默认同一个插件的 handler 永不并发，这是 ABI 的硬约定。但联网这类
        /// 慢调用会把后面所有调用一起堵住（模组页在搜索时，切到游戏页要等
        /// 它跑完）。所以插件可以点名**确定不碰共享状态**的服务，宿主对
        /// 这些服务不再排闸门。
        #[serde(default)]
        pub concurrent: Vec<String>,
    }

    impl Manifest {
        pub fn from_slice(bytes: &[u8]) -> Result<Self, serde_json::Error> {
            serde_json::from_slice(bytes)
        }
    }
}

#[cfg(feature = "manifest")]
pub use manifest_impl::Manifest;

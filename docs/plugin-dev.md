# MIYLauncher 插件开发指南

> 适用：宿主 ABI **v1（revision 2）**。
> 与 `crates/miy-abi/src/lib.rs`、`include/miy_plugin.h`、`crates/miy-host/src/bus.rs` 保持同步。
> 三者不一致时，以代码为准；请把它们当成同一份契约的不同呈现。

---

## 0. 插件能做什么

MIYLauncher 的宿主只做四件事：开窗口（WebView2 + 原生外壳）、加载插件、转调服务、存取设置。其余全部由插件贡献。

一个插件可以：

| 能力 | 入口 | 说明 |
| --- | --- | --- |
| 提供服务 | `dispatch` 里按服务名分发 | 宿主、其它插件、你自己的页面都能 `call` |
| 调用宿主 | `host.call` | 设置、网络、文件选择、登录、剪贴板、打开目录 |
| 发事件 | `host.emit` | 入队后由 UI 线程派发，页面可实时订阅 |
| 起后台任务 | `host.spawn` | 协作式取消，不会被强杀 |
| 贡献设置项 | `manifest.contributes["settings.schema"]` | 宿主渲染表单、校验、落盘 |
| 贡献页面 | `manifest.contributes["ui.pages"]` + `ui/*.html` | 跑在自己的 iframe 里，可调服务 |

不能做（v1 的明确边界）：

- **不能热重载**。改了 dll 必须重启启动器。
- **不能隔离**。插件与宿主同进程、同权限。防护只有两样：panic/异常在边界内被挡住、访问冲突后该插件被熔断停用（重启恢复）。
- **不能占用别人的命名空间**。你只能提供 `<你的 id>.` 开头的服务，越界的请求宿主根本不会路由给你。
- **不能自带 UI 框架或网络栈**。页面在 `sandbox="allow-scripts"` 的 iframe 里，联网请走 `miy.net.*`。

---

## 1. 五分钟：第一个插件

以 Rust 为例（其它语言见 §13）。

### 1.1 `Cargo.toml`

```toml
[package]
name = "miy-hello"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
miy-abi = { path = "../../crates/miy-abi" }   # 或把 crates/miy-abi 拷进你的仓库

[profile.release]
panic = "unwind"       # 必须能被 catch_unwind 抓住；不要用 abort
opt-level = "z"
lto = true
strip = "symbols"
```

`.cargo/config.toml`：

```toml
[target.x86_64-pc-windows-msvc]
rustflags = ["-C", "target-feature=+crt-static"]
```

> `+crt-static` 不是优化项，是正确性项：否则别人的机器上会因为缺 `VCRUNTIME140.dll` 加载失败。
> `panic = "unwind"` 同理：`panic = "abort"` 下 `catch_unwind` 抓不住，一次 panic 会带走整个启动器。

### 1.2 `src/lib.rs`

```rust
use core::ffi::{c_char, c_void};
use miy_abi::*;

struct State {
    host: MiyHostV1,
}

const ID: &[u8] = b"miy.hello\0";
const VERSION: &[u8] = b"0.1.0\0";

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
    MIY_OK
}

unsafe extern "C" fn stop(state: *mut c_void) {
    if state.is_null() {
        return;
    }
    let st = Box::from_raw(state as *mut State);
    drop(st);                    // 这里是最后能安全使用 ctx 的时刻
}

unsafe fn dispatch_inner(
    state: *mut c_void,
    service: *const u8,
    service_len: usize,
    req: *const u8,
    req_len: usize,
    out: *mut *mut u8,
) -> i32 {
    if state.is_null() || out.is_null() {
        return MIY_ERR_INVALID_ARG;
    }
    let st = &*(state as *const State);
    let name = core::str::from_utf8_unchecked(core::slice::from_raw_parts(service, service_len));
    let body: &[u8] = if req.is_null() || req_len == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(req, req_len)
    };

    let payload: Vec<u8> = match name {
        "miy.hello.ping" => br#"{"pong":true}"#.to_vec(),
        "miy.hello.echo" => body.to_vec(),
        "miy.hello.info" => {
            // 反向调用：handler 里调宿主服务是允许的
            let info = st.host.call_json("miy.app.info", b"").unwrap_or_default();
            info
        }
        _ => return MIY_ERR_UNKNOWN_SERVICE,
    };

    let buf = st.host.alloc_buf(&payload);   // 必须用宿主的分配器
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
    // panic 必须在边界内挡住
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
```

`PluginEntryCell` 存在的唯一原因是：`MiyPluginV1` 里有裸指针，不是 `Sync`，不能直接当 `static`。它只做 `#[repr(transparent)]` 包装 + `unsafe impl Sync`。

### 1.3 `manifest.json`

```json
{
  "id": "miy.hello",
  "name": "Hello",
  "version": "0.1.0",
  "description": "最小示例：ping / echo / info",
  "abi": 1,
  "entry": "miy_hello.dll",
  "priority": 0,
  "contributes": {}
}
```

### 1.4 构建与安装

```powershell
cargo build --release
# 产物：target\release\miy_hello.dll

# 装进启动器
mkdir dist\plugins\miy.hello
copy target\release\miy_hello.dll dist\plugins\miy.hello\
copy manifest.json               dist\plugins\miy.hello\
```

启动启动器，左侧「插件」页应该出现 `miy.hello 0.1.0 已启用`。

### 1.5 验证

- 界面：插件页会列出所有已加载插件，以及每个的调用次数。
- 命令行：`MIYLauncher.exe --spike` 会做一轮完整自检（加载插件、调 `miy.hello.ping`、跑超时/取消/熔断用例）。
- 日志：`<exe 目录>\MIYLauncher\logs\launcher.log`（所有数据都待在 exe 旁边，不写 `%LOCALAPPDATA%`、也不写 `%TEMP%`）。

---

## 2. 插件包结构

```
plugins/
  miy.hello/
    manifest.json          必需
    miy_hello.dll          必需，名字由 manifest.entry 决定
    ui/                    可选：页面
      page.html
    assets/                可选：你自己的资源（页面里用相对路径引用）
```

- 插件目录名建议用 `id`，但宿主只认 `manifest.json`。
- 加载目录：exe 旁边的 `plugins\`。只有开发期（exe 在 `target\{debug,release}` 里）才会退到当前工作目录的 `plugins\`，发行版不会去 cwd 里捡插件。
- 宿主启动时在后台线程扫描全部 `plugins/*/manifest.json`，按目录名排序加载（`priority` 字段保留、暂未参与），不占用 UI 线程。
- 单个插件加载失败不影响其它插件；失败原因会出现在插件页与日志里。
- 运行时目录一律在 `<exe 目录>\MIYLauncher\` 下：`config\`（设置、凭据）、`accounts\`、`logs\`、`webview2\`（界面缓存）、`temp\`、`java\`（自动下载的 JRE）。插件要落盘的东西也请放这里 —— 启动器不写 exe 同级以外的任何位置（包括 `%TEMP%`、`%LOCALAPPDATA%`）。

> **内置插件**：`miy.mc`（版本清单 / 下载 / 启动）是编译进 `MIYLauncher.exe` 的
> （`crates/miy-host/src/embedded.rs`，代码以 rlib 静态链接）。所以发行包里没有
> `plugins\miy.mc\`，`dist\MIYLauncher.exe` 单文件就能装并启动游戏。老用户升级后
> 留下的 `plugins\miy.mc` 会被识别并跳过（日志：`已内置在 exe 里，磁盘上的旧版本被跳过`）。
>

---

## 3. `manifest.json` 字段

| 字段 | 类型 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `id` | string | ✅ | — | 唯一标识，只允许 `A-Z a-z 0-9 . - _`。它决定服务命名空间、设置项前缀、页面归属 |
| `name` | string | | = `id` | 展示名 |
| `version` | string | | `""` | 展示用，宿主不解析 |
| `description` | string | | `""` | 展示用 |
| `author` | string | | `""` | 展示用 |
| `abi` | number | | `1` | 必须等于宿主的 `MIY_ABI_VERSION`，否则拒绝加载 |
| `entry` | string | | `<id>.dll` | 相对插件目录的 dll 路径 |
| `enabled` | bool | | `true` | 预留，当前宿主不读 |
| `priority` | number | | `0` | 预留（加载顺序） |
| `eager` | bool | | `true` | 预留（懒加载） |
| `contributes` | object | | `{}` | `settings.schema` / `ui.pages` |
| `concurrent` | string[] | | `[]` | 允许并发的服务后缀，见 §11 |

`id` 里带点是常态：`miy.mc` 的服务叫 `miy.mc.versions`、设置项叫 `miy.mc.root`。

---

## 4. ABI 契约

### 4.1 入口

```rust
#[no_mangle] pub extern "C" fn miy_plugin_entry_v1() -> *const MiyPluginV1
```

C/C++ 里就是：

```c
extern "C" __declspec(dllexport) const miy_plugin_v1* miy_plugin_entry_v1(void);
```

宿主用 `GetProcAddress` 找这个**精确**符号。C++ 忘了 `extern "C"`、Rust 忘了 `#[no_mangle]`，都会得到“找不到入口”的加载失败。

### 4.2 `MiyPluginV1`

| 偏移（x64） | 字段 | 类型 | 说明 |
| --- | --- | --- | --- |
| 0 | `abi_version` | `uint32` | 必须是 `1` |
| 4 | `struct_size` | `uint32` | `sizeof(MiyPluginV1)`；宿主用它判断哪些字段可用 |
| 8 | `id` | `const char*` | NUL 结尾 UTF-8，必须与 `manifest.id` 一致 |
| 16 | `version` | `const char*` | NUL 结尾 UTF-8 |
| 24 | `start` | `int32 (*)(const MiyHostV1*, void** out_state)` | 返回 `MIY_OK` 才算起来 |
| 32 | `stop` | `void (*)(void* state)` | 返回后 ctx 立即吊销 |
| 40 | `dispatch` | `int32 (*)(void* state, const uint8_t* service, size_t slen, const uint8_t* req, size_t rlen, uint8_t** out)` | 提供服务的唯一入口 |

**只追加原则**：以后新字段一律加在表尾，靠 `struct_size` 判断。宿主读字段时用 `read_unaligned` 且不整体解引用，所以 `struct_size` 小一点的旧插件照样能跑（只是新字段不可用）。

`struct_size < 40` 会被当成“没有 `dispatch`”，等于不能提供任何服务。

### 4.3 `MiyHostV1`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `abi_version` / `struct_size` | `uint32` | 同上一节 |
| `ctx` | `void*` | 宿主上下文，所有回调的第一个参数 |
| `log` | `(ctx, int32 level, const uint8_t* msg, size_t len)` | 写日志。等级：`0 trace / 1 debug / 2 info / 3 warn / 4 error` |
| `alloc` | `(ctx, size_t) -> uint8_t*` | 分配**带 4 字节长度前缀**的缓冲区 |
| `free` | `(ctx, uint8_t*)` | 释放上面那种缓冲区 |
| `call` | `(ctx, const uint8_t* service, size_t slen, const uint8_t* req, size_t rlen) -> uint8_t*` | 调服务；失败返回 null |
| `task` | `(ctx, entry, void* arg) -> uint64` | 起后台任务，返回任务 id（0 = 拒绝） |
| `emit` | `(ctx, const uint8_t* topic, size_t tlen, const uint8_t* payload, size_t plen)` | 发事件（入队） |

`miy-abi` 提供了一批便捷方法，建议直接用：

```rust
host.log_str(MIY_LOG_INFO, "hello");
host.log_bytes(MIY_LOG_WARN, body);
let out: *mut u8 = host.alloc_buf(b"{\"ok\":true}");
host.free_buf(out);
let reply: Option<Vec<u8>> = host.call_json("miy.app.info", b"");   // 自动 free
host.emit_str("miy.hello.tick", br#"{"n":1}"#);
let id: u64 = host.spawn(task_fn, Box::into_raw(Box::new(arg)).cast());
```

### 4.4 缓冲区与所有权

缓冲区布局固定：

```
+----------------+---------------------------+
| u32 LE  长度   |  载荷（长度个字节）        |
+----------------+---------------------------+
```

- 头 4 字节是小端 `u32`，表示载荷长度；上限 `MIY_BUF_MAX = 16 MiB`。
- 前缀的意义：`free(p)` 不需要额外参数就能还原整块布局。这就是“跨边界只许用宿主分配器”能成立的原因。

所有权一目了然：

| 数据 | 谁分配 | 谁释放 |
| --- | --- | --- |
| 插件写进 `*out` 的返回缓冲区 | 插件用 `host.alloc_buf` | **宿主** `free` |
| `host.call` 返回的缓冲区 | 宿主 | **插件** `host.free_buf` |
| `service` / `req` / `topic` 参数 | 调用方 | 调用方，被调用方只借用 |

**绝不要**把 Rust `Vec` / C `malloc` 的指针交给对面，反之亦然。分配器不匹配的崩溃现场通常是一句 `heap corruption`，很难查。

### 4.5 错误码

| 常量 | 值 | 含义 |
| --- | --- | --- |
| `MIY_OK` | 0 | 成功 |
| `MIY_ERR_INVALID_ARG` | −1 | 参数非法（空指针、超长、非法 UTF-8） |
| `MIY_ERR_ABI` | −2 | ABI 不兼容 |
| `MIY_ERR_UNKNOWN_SERVICE` | −3 | 这个服务名我不提供 |
| `MIY_ERR_PANIC` | −4 | 插件侧 panic/异常被边界挡住 |
| `MIY_ERR_REENTRANT` | −5 | 重入被拒（见 §6.2） |
| `MIY_ERR_TIMEOUT` | −6 | 超时（宿主放弃等待，不杀线程） |
| `MIY_ERR_NOT_FOUND` | −7 | 找不到服务/插件 |
| `MIY_ERR_FAULT` | −8 | 访问冲突等硬件异常；该插件会被熔断 |

`MIY_FAULT_SENTINEL = 0x7A11` 是 C 侧 SEH shim 与 Rust 侧约定的哨兵值。

### 4.6 生命周期

```
LoadLibrary → GetProcAddress(miy_plugin_entry_v1) → 校验表 → start(host, &state)
   ↓
   dispatch(service, req, &out)   ← 任意线程，同插件默认串行
   ↓
stop(state) → 吊销 ctx → FreeLibrary
```

- `start` 返回非 `MIY_OK` 视为加载失败，宿主不会调 `stop`。
- `stop` 之后 `ctx` 立即作废：再 `log` / `call` / `emit` 都是未定义行为。
- 宿主退出时按相反顺序 `stop` 全部插件，并给 5 秒等当前 handler 收尾。

---

## 5. 六条硬规则

1. **跨边界只传 UTF-8 + 显式长度**。`id`/`version` 是 NUL 结尾 C 串，其余（服务名、请求、返回）都是「指针 + 长度」。
2. **只使用宿主的分配器**。返回给宿主的内存必须来自 `host.alloc_buf`。
3. **`stop` 返回后不再碰 ctx**。
4. **`emit` 是入队，不是同步回调**。顺序有保证，时机由 UI 线程决定。
5. **取消是协作式**。宿主只把令牌立起来，绝不 `TerminateThread`；置位之后你的任务多跑一轮是合法的。
6. **panic / 异常不能越过 FFI**。Rust 用 `catch_panic`，C++ 用 `catch (...)` 包住整个 `dispatch`，C 由宿主的 SEH shim 兜底。

---

## 6. 调用宿主

### 6.1 语义

`host.call(service, req)` 是**同步**调用：

- 有 deadline，默认 **5 秒**；超时返回 `{"code":"timeout"}`，你的线程不会被杀，但这次调用拿不到结果。
- 失败不抛异常，而是返回一个带 `code` 的 JSON：
  `{"code":"reentrant"}`、`{"code":"timeout"}`、`{"code":"not-found"}`、`{"code":"error","err":-4}`。
- 请求与返回都是任意字节；约定用 JSON（宿主不强制解析，但页面与其它插件默认按 JSON 消费）。

### 6.2 重入规则

- **handler 里可以调宿主服务**（`miy.app.*` / `miy.net.*` / `miy.accounts.*`）。宿主服务是普通函数调用，不占派发池，不会死锁。
- **handler 里不能再调插件服务**（包括你自己的）。返回 `{"code":"reentrant"}`。

原因很实际：派发池只有 8 条线程，若 handler 之间可以互相等待，四个插件就能把池子卡死。

### 6.3 宿主服务一览

#### 应用

| 服务 | 请求 | 返回 |
| --- | --- | --- |
| `miy.app.ping` | — | `{"pong":true}` |
| `miy.app.info` | — | `{version, abi, revision, dataRoot, pluginsDir, exeDir, log, window:{width, height}, memory:{total, avail}}`（`window` 是主窗口客户区的物理像素，窗口还没建好时是 0；`memory` 单位 MB，`avail` 是当前可用物理内存） |
| `miy.app.plugins` | — | `{plugins:[{id, version, started, faulted, calls, faults, timeouts, dir, pages}]}` |
| `miy.app.pages` | — | `{pages:[{plugin, id, title, slot, order}]}` |
| `miy.app.page` | `{plugin, id}` | `{ok, plugin, id, title, html}` / `{ok:false, error}` |
| `miy.app.log` | — | `{text}`（日志尾部约 24 KiB） |
| `miy.app.open` | `{url}` 或 `{what:"logs"\|"plugins"\|"game"\|"data"}` | `{ok}` |
| `miy.app.clipboard` | `{text}` | `{ok, chars}` |
| `miy.app.pickFolder` | `{title}` | `{ok, path}` / `{ok:false, error:"cancelled"}` |

`miy.app.window`（`{action}`）只给核心界面用，插件不要调。

#### 设置

| 服务 | 请求 | 返回 |
| --- | --- | --- |
| `miy.app.settings` | — | `{schema:[...], values:{key: value}}` |
| `miy.app.setting.set` | `{key, value}` | `{ok, key, value}` / `{ok:false, error}` |
| `miy.app.setting.reset` | `{key}` | `{ok, key, value}` |

读自己设置的推荐写法：

```rust
let snap = host.call_json("miy.app.settings", b"")?;
let v: serde_json::Value = serde_json::from_slice(&snap)?;
let root = v["values"]["miy.mc.root"].as_str().unwrap_or("");
```

#### 网络

TLS 走系统 WinHTTP/SChannel，插件不需要（也不应该）自带 TLS 栈。

| 服务 | 请求 | 返回 |
| --- | --- | --- |
| `miy.net.get` | `{url, timeout_ms?, max_bytes?}` | `{ok, status, text, bytes}` / `{ok:false, error}` |
| `miy.net.post` | `{url, body, content_type?, timeout_ms?}` | 同上 |
| `miy.net.download` | `{url, path, timeout_ms?}` | `{ok, bytes}`，并发事件 `net.downloaded` |

`miy.net.download` 会写到 `<path>.part` 再改名，中途失败不会留下半截文件。

#### 账号

| 服务 | 说明 |
| --- | --- |
| `miy.accounts.list` | 已保存账号与当前账号 |
| `miy.accounts.check` | 自检（客户端 ID / 登录端点 / 公共客户端流是否配好） |
| `miy.accounts.start` | 申请设备码，返回 `{ok, user_code, verification_uri, ...}` |
| `miy.accounts.poll` | 轮询一次，返回 `{"status":"pending"\|"ok"\|"expired"\|"declined"\|"idle", ...}` |
| `miy.accounts.cancel` | 取消当前登录 |
| `miy.accounts.logout` | 退出登录 |
| `miy.accounts.switch` | 切换当前账号：`{"id":"<uuid>"}` |
| `miy.accounts.remove` | 从列表里删掉一个账号：`{"id":"<uuid>"}` |
| `miy.accounts.token` | 取当前账号的 MC 访问令牌 —— **只在启动游戏时用**，别缓存、别外传 |

---

## 7. 事件

```rust
host.emit_str("miy.hello.tick", br#"{"n":1}"#);
```

- 主题命名：`<你的 id>.<事件名>`，例如 `miy.mc.progress`。
- 入队后由 UI 线程派发；队列上限 4096 条，满了丢最旧的（宁可丢进度，不要爆内存）。
- 页面里订阅：

```js
miy.onEvent((topic, payload) => {
  if (topic === "miy.hello.tick") render(payload);
});
```

宿主自己也会发事件，可以订阅它们做联动：

| 主题 | 载荷 |
| --- | --- |
| `plugin.loaded` / `plugin.failed` | 加载结果 |
| `plugin.ready` | 全部加载结束 |
| `plugins.loaded` | `{ok, total}` |
| `settings.changed` | `{key, value}` |
| `accounts.changed` | `{name}` |
| `net.downloaded` | `{path, bytes, ms}` |
| `host.error` | `{message}` |
| `window.state` | `{maximized}` |

---

## 8. 后台任务

```rust
unsafe extern "C" fn tick(arg: *mut c_void, cancel: *const MiyCancel) -> i32 {
    let a = Box::from_raw(arg as *mut TaskArg);   // 所有权在任务体
    loop {
        if !cancel.is_null() && (*cancel).is_cancelled() {
            return MIY_OK;
        }
        std::thread::sleep(std::time::Duration::from_millis(a.interval_ms));
        a.host.emit_str("miy.hello.tick", b"{}");
    }
}

let id = host.spawn(tick, Box::into_raw(Box::new(arg)).cast());
```

要点：

- `arg` 的所有权转交给任务体，**由任务体负责回收**（通常是 `Box::from_raw`）。宿主不会替你释放。
- `cancel` 指向宿主持有的令牌，非空且 `is_cancelled()` 为真时尽快退出。
- 只能在安全点退出：`sleep` 中间被打断不行，检查要在循环体开头。
- 任务退出后不要再用 `arg`。
- 在任务里调服务是允许的（不算 handler 重入），但**不要调用会等待该任务完成的那个服务**，否则互相卡住。

---

## 9. 设置项

插件只贡献 **schema**，不写 UI：宿主负责合并、渲染、校验、落盘。

```json
"contributes": {
  "settings.schema": {
    "root": {
      "type": "string",
      "title": "游戏目录",
      "description": "留空则用启动器旁边的 MC 目录。",
      "default": "",
      "group": "通用",
      "order": 0
    },
    "concurrency": {
      "type": "int",
      "title": "同时下载数",
      "min": 1,
      "max": 16,
      "default": 4,
      "group": "通用",
      "order": 1
    }
  }
}
```

也可以用数组形式：`"settings.schema": [{ "key": "root", "type": "string", ... }]`。

| 字段 | 说明 |
| --- | --- |
| `type` | `bool` / `int` / `float` / `number` / `enum` / `string`（缺省 `string`） |
| `title` | 显示名 |
| `description` | 一行说明 |
| `default` | 默认值（必填，决定“恢复默认”回到哪） |
| `enum` | `enum` 类型的候选值数组 |
| `min` / `max` | `int` / `float` / `number` 的取值范围 |
| `group` | 分组标题；不填则归到插件名 |
| `order` | 组内排序，小的在前 |
| `control` | 渲染控件：`slider` 用滑杆（配 `step`，上限按本机物理内存封顶），不填按类型自动选 |
| `step` | 滑杆步长（`control: "slider"` 时用） |

**键名规则**：宿主自动加前缀。写 `root` 得到 `miy.mc.root`；如果你已经写成 `miy.mc.root`，宿主不会再叠一层。

**存储**：`<data>\config\settings.json` 的 `values` 下，未知键原样保留（插件降级/回滚不丢配置）。默认值不写入文件，只有用户改过才有。

**变更通知**：任何一项被改，宿主发 `settings.changed`。插件如果要立即响应（比如改并发数），订阅它，或在下一次调用时重新读 `miy.app.settings`。

---

## 10. 插件页面

```json
"contributes": {
  "ui.pages": [
    { "id": "home",  "title": "首页", "slot": "home", "entry": "home.html",  "order": 0 },
    { "id": "mods",  "title": "模组", "slot": "nav",  "entry": "mods.html",  "order": 30 }
  ]
}
```

| 字段 | 说明 |
| --- | --- |
| `id` | 页面标识，在插件内唯一 |
| `title` | 导航文字 |
| `slot` | `home` = 启动后的首页；`nav` = 左侧导航项（缺省） |
| `entry` | 相对 `plugins/<id>/ui/` 的 HTML 文件（缺省 `page.html`） |
| `order` | 导航排序，小的在前 |

宿主把 HTML 读出来，插进 `iframe`（`sandbox="allow-scripts"`，通过 `srcdoc` 注入），并自动加上页面桥：

```js
// 页面里可用的全部 API
miy.call(service, req, timeoutMs)   // 返回 Promise；默认超时 20000ms
miy.onEvent(fn)                     // fn(topic, payload)
miy.ready                           // true
```

失败时 Promise 会 reject 一个带 `.code` 的错误：

```js
try {
  await miy.call("miy.mc.versions", {});
} catch (e) {
  if (e.code === "timeout")  status("网络有点慢，重试中…");
  if (e.code === "forbidden") status("这个页面没有权限");
}
```

### 10.1 权限

页面只能调用：

1. 你自己的服务（`<你的 id>.` 开头）；
2. 宿主白名单：`miy.app.info`、`miy.app.log`、`miy.app.pages`、`miy.app.settings`、`miy.app.open`、`miy.app.clipboard`、`miy.app.pickFolder`，以及 `miy.accounts.list / check / start / poll / cancel / logout / switch / remove`。

越权返回 `{"code":"forbidden"}`，并在控制台留一条痕迹。这层不是沙箱（页面背后是你自己的原生代码），它只是防止页面“顺手”读到不该读的东西。

### 10.2 页面要注意的事

- **独立文档**：拿不到宿主的 CSS 变量、`localStorage`、cookie。样式自己写全。
- **不要直接联网**：`fetch` 在 `sandbox` 的 opaque origin 下会被 CORS 拦；一律走 `miy.call("miy.net.get", …)`。
- **路径护栏**：`entry` 里出现 `..`、`:`、或以 `/`、`\` 开头会被拒绝加载。
- **状态要能重建**：宿主会缓存已打开过的 iframe（切页不重建），但刷新/切换账号时页面会被重新创建，别把关键状态只放在内存里。
- **长任务不要卡在页面里**：超过 20 秒的调用会被桥判超时。下载/安装这类事请让插件起后台任务，页面订阅事件看进度。

---

## 11. 并发声明

默认规则很简单：**同一个插件同一时刻只有一个 handler 在跑**。这替你省掉了所有内部加锁，也让“设置读一半被改”这类问题不存在。

但有些服务是纯读的、且可能很慢（比如一次 30 秒的网络搜索）。它们会把整个插件堵住，页面切到别处也会一起排队。这类服务可以声明成并发：

```json
"concurrent": ["versions", "installed", "info", "mods", "mod", "mod.versions", "tasks"]
```

- 数组里写的是**服务后缀**（`<你的 id>.` 之后的部分），宿主会先补成完整名 `<id>.<后缀>` 再**精确比较**。所以 `"mod"` 只放行 `miy.mc.mod`，不会顺带放行 `miy.mc.mod.install`。
- 只把**无共享状态**的读接口放进来。写文件、改全局状态的接口留在默认串行里。
- 并发的 handler 之间共享你的 `state`，需要你自己保证线程安全。

---

## 12. 调试与排错

### 12.1 日志

```rust
host.log_str(MIY_LOG_INFO,  "加载完成");
host.log_str(MIY_LOG_WARN,  "镜像返回 403，换下一个");
host.log_str(MIY_LOG_ERROR, "写文件失败");
```

落盘在 `<data>\logs\launcher.log`，超过 1 MiB 自动轮转。设置页把「日志级别」调到 `debug` 能看到更细的东西。

### 12.2 常用命令

```powershell
MIYLauncher.exe --spike        # 自检：插件握手/调用/超时/取消/熔断/设置
MIYLauncher.exe --safe-mode    # 跳过全部插件启动（排查“是不是插件的锅”）
MIYLauncher.exe --check-webview
MIYLauncher.exe --help
```

### 12.3 症状对照表

| 症状 | 多半是 | 处理 |
| --- | --- | --- |
| 插件页显示「已安装」但没启用 | ABI 不符 / 缺导出符号 / dll 加载失败 | 看日志首行，通常是找不到 `miy_plugin_entry_v1` |
| 调服务返回 `{"code":"not-found"}` | 服务名没以 `<id>.` 开头，或插件没起来 | 核对 `id` 与服务名前缀 |
| 返回 `{"code":"timeout"}` | 超过 5 秒 | 拆成任务 + 事件，或声明并发 |
| 返回 `{"code":"reentrant"}` | handler 里调了插件服务 | 改成“先返回结果，再异步编排” |
| 启动器直接消失 | panic 越过 FFI，或访问冲突 | 看 `logs\crash-*.dmp`；补 `catch_panic` |
| 插件页显示「已停用」 | 曾触发硬件异常被熔断 | 修好重启；熔断状态不跨进程保留 |
| 别人机器上 `VCRUNTIME140.dll` 缺失 | 没静态链接 CRT | 加 `+crt-static` |
| `Entry Point Not Found` | 导出名不对（C++ 没 `extern "C"`） | 用 `dumpbin /exports` 自查 |
| 页面白屏 | 页面 JS 抛异常，`boot()` 没跑完 | 页面里装 `window.onerror` 把错误显示出来 |

### 12.4 自检插件是否被加载

```powershell
MIYLauncher.exe --spike
# [spike] miy.hello 加载成功（4493 µs）
# [spike] OK  ping: {"pong":true}
```

---

## 13. 用别的语言写

ABI 是纯 C 的，任何能产出 x64 DLL 的语言都行。

**C / C++**：直接 `#include "include/miy_plugin.h"`

```c
extern "C" __declspec(dllexport)
const miy_plugin_v1* miy_plugin_entry_v1(void) {
    return &g_entry;
}
```

- 用 `/MT`（静态 CRT），不要 `/MD`。
- 整个 `dispatch` 外面包 `try { … } catch (...) { return MIY_ERR_PANIC; }`，编译时别用 `/EHs-` 把异常关掉。
- 返回值必须来自 `host->alloc`。

**C#**：NativeAOT + `[UnmanagedCallersOnly(CallConvs = new[] { typeof(CallConvCdecl) })]` 导出入口；`AllowUnsafeBlocks`；发布时 `PublishAot=true`。注意 AOT 剪裁会删掉没用到的导出，用 `UnmanagedCallersOnly` 标好即可保留。

**Zig**：

```zig
export fn miy_plugin_entry_v1() callconv(.c) *const MiyPluginV1 { return &entry; }
```

**Go**：`-buildmode=c-shared` 能出 DLL，但会把 Go runtime 整个塞进去（20 MB 起），且回调进 Go 的栈切换容易出事。不推荐，确实要用时把边界做成一进一出、别在 Go 里长时间持有 C 内存。

其它语言共同的三条：

1. 必须是 **x64**，位数不匹配直接加载失败。
2. 静态链接 CRT/运行时，别依赖开发机上的 DLL。
3. 异常/panic 一律在边界内消化。

---

## 14. 常见坑

1. **忘了 `#[no_mangle]` / `extern "C"`** → 导出名被 mangle，宿主导不到入口。
2. **用自己分配器返回内存** → 宿主 `free` 时堆损坏。只认 `host.alloc_buf`。
3. **`stop` 之后还用 ctx** → 宿主已吊销，属于 UB，通常表现为莫名其妙的崩溃。
4. **在 handler 里睡很久** → 占住该插件的闸门，同插件其它调用全部超时。
5. **以为 `emit` 是同步的** → 它是入队；不要在 emit 之后立刻假设界面已经更新。
6. **页面里 `fetch` 外网** → sandbox 下被拦，走 `miy.net.*`。
7. **页面依赖宿主 CSS 变量** → 拿不到，自己铺全套样式。
8. **把大 JSON 塞进事件** → 事件会广播给所有页面，几百 KB 的载荷会拖慢 UI。大块数据用服务按需取。
9. **用 `panic = "abort"`** → 一次 panic 直接结束启动器进程。
10. **改了 dll 没重启** → 没有热重载，这是设计决定。
11. **设置项 `default` 忘了写** → “恢复默认”没有落点，界面会出现 undefined。
12. **服务名大小写不一致** → 服务名精确匹配，`miy.Foo.ping` ≠ `miy.foo.ping`。

---

## 15. 版本与兼容

- ABI 只在**表尾追加**字段，靠 `struct_size` 做能力探测，所以旧插件在新宿主上永远能跑。
- `MIY_ABI_REVISION` 表示语义级变化（rev2：任务体多了 `cancel` 参数）。它不影响加载，只用于日志与自检输出。
- 新插件用到了新字段时，要自己判断 `host.struct_size` 是否够；不够就降级或直接返回 `MIY_ERR_ABI`。
- 旧宿主不能跑新插件（字段越界）——这是单向兼容，方向别搞反。

---

## 16. 提交前检查清单

- [ ] `manifest.json` 的 `id` 只含 `A-Z a-z 0-9 . - _`，与 dll 里 `MiyPluginV1.id` 完全一致
- [ ] `abi` = 1，`entry` 指向的 dll 真实存在
- [ ] 导出符号是 `miy_plugin_entry_v1`（`dumpbin /exports` 看过）
- [ ] `start` 校验了 `host.abi_version`，失败返回 `MIY_ERR_ABI`
- [ ] `dispatch` 外面包了 `catch_panic` / `try…catch(...)`
- [ ] 所有返回给宿主的缓冲区都来自 `host.alloc_buf`
- [ ] 未知服务返回 `MIY_ERR_UNKNOWN_SERVICE`
- [ ] 后台任务检查取消令牌，`arg` 由任务体回收
- [ ] 慢接口要么拆任务，要么进 `concurrent`
- [ ] 页面只调自己的服务和白名单，样式自包含
- [ ] 静态链接 CRT（`+crt-static` / `/MT`），x64
- [ ] 跑过一遍 `MIYLauncher.exe --spike`

//! miy.hello —— 最小的真实插件。
//!
//! 它只做三件事：证明 ABI 能握手、证明 `call` 能路由进插件、证明插件
//! 自己分配内存（用宿主的分配器）并把所有权交回去。

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
    h.log_str(MIY_LOG_INFO, "miy.hello: start");
    MIY_OK
}

unsafe extern "C" fn stop(state: *mut c_void) {
    if state.is_null() {
        return;
    }
    let st = Box::from_raw(state as *mut State);
    st.host.log_str(MIY_LOG_INFO, "miy.hello: stop");
    drop(st);
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
    let body: &[u8] = if req.is_null() || req_len == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(req, req_len)
    };

    let payload: Vec<u8> = match name {
        "miy.hello.ping" => {
            format!(
                r#"{{"pong":true,"plugin":"miy.hello","abi":{},"rev":{}}}"#,
                MIY_ABI_VERSION, MIY_ABI_REVISION
            )
            .into_bytes()
        }
        "miy.hello.echo" => body.to_vec(),
        // 顺便验证"插件调宿主"这条反向通道没有死锁
        "miy.hello.log" => {
            st.host.log_bytes(MIY_LOG_INFO, body);
            br#"{"logged":true}"#.to_vec()
        }
        // 负向：handler 内部再 call，必须被宿主拒绝而不是把池子锁死
        "miy.hello.reenter" => st
            .host
            .call_json("miy.hello.ping", b"")
            .unwrap_or_else(|| br#"{"code":"no-answer"}"#.to_vec()),
        // 耗时服务：验证"超时只放弃等待，线程不被杀，插件之后还能接客"
        "miy.hello.slow" => {
            let ms = parse_ms(body).min(60_000);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            format!(r#"{{"slept":{ms}}}"#).into_bytes()
        }
        // 事件通道：插件 emit 之后，宿主的 UI 线程应该能收到
        "miy.hello.emit" => {
            st.host.emit_str("miy.hello.tick", br#"{"n":1}"#);
            br#"{"emitted":true}"#.to_vec()
        }
        // 任务原语：起一个后台任务，返回任务 id。任务体自己盯着 cancel 令牌。
        "miy.hello.task" => {
            let interval = parse_ms(body).clamp(20, 10_000);
            let arg = Box::new(TaskArg {
                host: st.host,
                interval_ms: interval,
            });
            let id = st.host.spawn(task_tick, Box::into_raw(arg).cast());
            format!(r#"{{"task":{id}}}"#).into_bytes()
        }
        _ => return MIY_ERR_UNKNOWN_SERVICE,
    };

    let buf = st.host.alloc_buf(&payload);
    if buf.is_null() {
        return MIY_ERR_INVALID_ARG;
    }
    *out = buf;
    MIY_OK
}

struct TaskArg {
    host: MiyHostV1,
    interval_ms: u64,
}

/// 任务体：每 `interval_ms` 发一个 tick，直到宿主请求取消。
///
/// 这就是"协作式取消"的全部样子——没人能强杀这个线程，宿主只能把令牌立起来，
/// 由任务体自己决定什么时候体面退出。
unsafe extern "C" fn task_tick(arg: *mut c_void, cancel: *const MiyCancel) -> i32 {
    if arg.is_null() {
        return MIY_ERR_INVALID_ARG;
    }
    let a = Box::from_raw(arg as *mut TaskArg);
    let mut n: u32 = 0;
    loop {
        if !cancel.is_null() && (*cancel).is_cancelled() {
            a.host.emit_str(
                "miy.hello.task.cancelled",
                format!(r#"{{"ticks":{n}}}"#).as_bytes(),
            );
            return MIY_OK;
        }
        std::thread::sleep(std::time::Duration::from_millis(a.interval_ms));
        n += 1;
        a.host.emit_str(
            "miy.hello.task.tick",
            format!(r#"{{"n":{n}}}"#).as_bytes(),
        );
        // 上限只是防止一个跑飞的插件任务永远占着池子线程。
        if n >= 1000 {
            return MIY_OK;
        }
    }
}

/// 从 `{"ms":123}` 这种请求里抠出数字。这里不引 serde，保持插件零依赖。
fn parse_ms(body: &[u8]) -> u64 {    let text = core::str::from_utf8(body).unwrap_or("");
    let Some(pos) = text.find("\"ms\"") else {
        return 100;
    };
    let rest = &text[pos + 4..];
    let digits: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap_or(100)
}

unsafe extern "C" fn dispatch(
    state: *mut c_void,
    service: *const u8,
    service_len: usize,
    req: *const u8,
    req_len: usize,
    out: *mut *mut u8,
) -> i32 {
    // panic 在边界内被挡住，绝不越过 FFI
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

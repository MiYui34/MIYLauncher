//! miy.boom —— 负向测试插件。
//!
//! 提供两种"炸法"，对应两条隔离边界：
//! * `miy.boom.panic`：Rust panic，必须在插件内部被 catch_unwind 挡住；
//! * `miy.boom.crash`：真实访问冲突，只能由宿主侧 SEH 兜住。

use core::ffi::{c_char, c_void};
use miy_abi::*;

struct State {
    host: MiyHostV1,
}

const ID: &[u8] = b"miy.boom\0";
const VERSION: &[u8] = b"0.1.0\0";

unsafe extern "C" fn start(host: *const MiyHostV1, out_state: *mut *mut c_void) -> i32 {
    if host.is_null() || out_state.is_null() {
        return MIY_ERR_INVALID_ARG;
    }
    let st = Box::new(State { host: *host });
    *out_state = Box::into_raw(st).cast();
    MIY_OK
}

unsafe extern "C" fn stop(state: *mut c_void) {
    if !state.is_null() {
        drop(Box::from_raw(state as *mut State));
    }
}

/// 写一个非法地址。`volatile` 保证它不会被优化掉。
#[inline(never)]
fn crash_now() {
    unsafe {
        core::ptr::write_volatile(0x10 as *mut u64, 0xdead_beef);
    }
}

unsafe fn dispatch_inner(
    state: *mut c_void,
    service: *const u8,
    service_len: usize,
    _req: *const u8,
    _req_len: usize,
    out: *mut *mut u8,
) -> i32 {
    if state.is_null() || out.is_null() {
        return MIY_ERR_INVALID_ARG;
    }
    let st = &*(state as *const State);
    let name = core::str::from_utf8_unchecked(core::slice::from_raw_parts(service, service_len));

    match name {
        "miy.boom.panic" => panic!("miy.boom: 故意 panic"),
        "miy.boom.crash" => {
            crash_now();
            // 走不到这里
            MIY_OK
        }
        "miy.boom.alive" => {
            let buf = st.host.alloc_buf(br#"{"alive":true}"#);
            if buf.is_null() {
                return MIY_ERR_INVALID_ARG;
            }
            *out = buf;
            MIY_OK
        }
        _ => MIY_ERR_UNKNOWN_SERVICE,
    }
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

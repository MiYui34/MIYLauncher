//! 跨边界的异常屏障 + 进程级崩溃兜底。
//!
//! 每一个进入插件（或从插件进入宿主）的调用都必须经过 [`guard`]。
//! 它接住的是硬件异常（访问冲突、除零、非法指令），不是 Rust panic ——
//! panic 由插件自己在边界内部 `catch_unwind` 处理。
//!
//! [`install_process_defaults`] 另外装一个"最后一道网"：真到了宿主自己都在崩的
//! 地步，至少留下一个转储文件和一行遗言，而不是让窗口默默消失。

use core::ffi::c_void;

use miy_abi::{MIY_ERR_FAULT, MIY_FAULT_SENTINEL};

extern "C" {
    fn miy_guard(f: Option<unsafe extern "C" fn(*mut c_void) -> i32>, arg: *mut c_void) -> i32;
}

unsafe extern "C" fn trampoline(arg: *mut c_void) -> i32 {
    let f = &mut *(arg as *mut Box<dyn FnMut() -> i32>);
    f()
}

/// 执行 `f`，把硬件异常翻译成 [`MIY_ERR_FAULT`]。
///
/// 注意：异常路径上 `f` 捕获的栈变量不会被析构（栈已经被 SEH 展开跳过），
/// 所以这里的 `Box` 会泄漏一次。一次崩溃换一次泄漏，值得。
pub fn guard<F: FnMut() -> i32>(f: F) -> i32 {
    let mut boxed: Box<dyn FnMut() -> i32> = Box::new(f);
    let arg = &mut boxed as *mut Box<dyn FnMut() -> i32> as *mut c_void;
    let rc = unsafe { miy_guard(Some(trampoline), arg) };
    if rc == MIY_FAULT_SENTINEL {
        MIY_ERR_FAULT
    } else {
        rc
    }
}

/// 进程级默认值：DLL 只从应用目录和 System32 找（防 PATH 劫持），
/// 未捕获异常写一份 minidump 并留一行遗言。
pub unsafe fn install_process_defaults() {
    use windows_sys::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter;
    use windows_sys::Win32::System::LibraryLoader::{
        SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_USER_DIRS,
    };

    SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS | LOAD_LIBRARY_SEARCH_USER_DIRS);
    SetUnhandledExceptionFilter(Some(unhandled));
}

/// 崩溃时留一份能在 WinDbg / VS 里打开的转储。
///
/// 这里刻意只做最少的动作：写文件、写一行文本、返回。异常过滤器运行在已经
/// 不可信的状态下，多一个分配就多一次二次崩溃。
unsafe fn write_crash_dump(
    code: u32,
    info: *const windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> Option<std::path::PathBuf> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{
        MiniDumpNormal, MiniDumpWriteDump, MINIDUMP_EXCEPTION_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId};

    let dir = crate::paths::logs_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("crash-{stamp}-{code:08x}.dmp"));

    // 宽字符路径：CreateFileW 不认 UTF-8，本机用户名可能是中文。
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let file = CreateFileW(
        wide.as_ptr(),
        GENERIC_WRITE,
        FILE_SHARE_READ,
        std::ptr::null(),
        CREATE_ALWAYS,
        FILE_ATTRIBUTE_NORMAL,
        std::ptr::null_mut(),
    );
    if file.is_null() || file == INVALID_HANDLE_VALUE {
        return None;
    }

    let mut mei = MINIDUMP_EXCEPTION_INFORMATION {
        ThreadId: GetCurrentThreadId(),
        ExceptionPointers: info as *mut _,
        ClientPointers: 0,
    };
    let ok = MiniDumpWriteDump(
        GetCurrentProcess(),
        GetCurrentProcessId(),
        file,
        MiniDumpNormal,
        if info.is_null() {
            std::ptr::null()
        } else {
            &mut mei
        },
        std::ptr::null(),
        std::ptr::null(),
    );
    CloseHandle(file);
    if ok != 0 {
        Some(path)
    } else {
        None
    }
}

unsafe extern "system" fn unhandled(
    info: *const windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> i32 {
    use windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_EXECUTE_HANDLER;

    let code = if info.is_null() {
        0
    } else {
        // NTSTATUS 是有符号的，转储文件名里按无符号写更好认。
        (*(*info).ExceptionRecord).ExceptionCode as u32
    };
    let addr = if info.is_null() {
        0
    } else {
        (*(*info).ExceptionRecord).ExceptionAddress as usize
    };

    let dump = write_crash_dump(code, info);
    match &dump {
        Some(p) => eprintln!("[fatal] 宿主未捕获异常 0x{code:08X} @ 0x{addr:016X}，转储 {}", p.display()),
        None => eprintln!("[fatal] 宿主未捕获异常 0x{code:08X} @ 0x{addr:016X}，转储写入失败"),
    }
    // 下次启动由 main 读走这条 → 页面弹出"上次崩了"。
    let dir = crate::paths::logs_dir();
    let _ = std::fs::create_dir_all(&dir);
    let note = match &dump {
        Some(p) => format!("0x{code:08X}\t{}\n", p.display()),
        None => format!("0x{code:08X}\t(转储失败)\n"),
    };
    let _ = std::fs::write(dir.join("crash-pending.txt"), note);
    // 交给系统终止 —— 我们不假装还能继续。
    EXCEPTION_EXECUTE_HANDLER
}

/// 上次启动是不是崩着退出的？返回给页面用的可读文案。
///
/// 读完就删：只报一次，不能每次启动都在喊"上次崩了"。
pub fn take_pending_crash() -> Option<String> {
    let path = crate::paths::logs_dir().join("crash-pending.txt");
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let mut parts = text.trim().split('\t');
    let code = parts.next().unwrap_or("?").trim().to_string();
    let dump = parts.next().unwrap_or("").trim().to_string();
    Some(if dump.is_empty() {
        format!("上次启动崩溃了（{code}），没有留下转储文件。")
    } else {
        format!("上次启动崩溃了（{code}），转储已保存在 {dump}")
    })
}

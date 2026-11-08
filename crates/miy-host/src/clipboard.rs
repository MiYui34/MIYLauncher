//! 往系统剪贴板放一段文本。
//!
//! 为什么需要它：插件页面跑在 `sandbox="allow-scripts"` 的 iframe 里，
//! 那种文档不是安全上下文，`navigator.clipboard.writeText` 直接不存在；
//! `document.execCommand("copy")` 也只对同源文档里选中的内容有效。
//! 与其给 iframe 放宽沙箱（那会把整个权限模型撕开一道口子），
//! 不如让宿主代劳——页面只剩一句 `miy.call("miy.app.clipboard", {text})`。

use windows_sys::Win32::Foundation::GlobalFree;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

/// 把 `text` 放进剪贴板（CF_UNICODETEXT）。
pub fn set_text(text: &str) -> Result<(), String> {
    if text.contains('\0') {
        return Err("文本里有 NUL，剪贴板放不进去".into());
    }
    // 窗口标题/剪贴板都是 UTF-16 + 结尾 NUL。
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * std::mem::size_of::<u16>();

    unsafe {
        // hwnd 传 null：剪贴板归当前任务，不绑某个窗口。
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("打不开剪贴板（可能被别的程序占着）".into());
        }
        // 从这里开始每一条失败路径都要 CloseClipboard，否则整台机器的
        // 复制粘贴会一直卡住，直到本进程退出。
        let _ = EmptyClipboard();

        let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if h.is_null() {
            let _ = CloseClipboard();
            return Err("分配剪贴板内存失败".into());
        }
        let dst = GlobalLock(h) as *mut u16;
        if dst.is_null() {
            let _ = GlobalFree(h);
            let _ = CloseClipboard();
            return Err("锁定剪贴板内存失败".into());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
        let _ = GlobalUnlock(h);

        if SetClipboardData(CF_UNICODETEXT as u32, h).is_null() {
            let _ = GlobalFree(h);
            let _ = CloseClipboard();
            return Err("写入剪贴板失败".into());
        }
        // 成功时内存归系统所有，**不能**再 GlobalFree。
        let _ = CloseClipboard();
    }
    Ok(())
}

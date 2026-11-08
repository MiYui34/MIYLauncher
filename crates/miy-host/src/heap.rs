//! 跨边界的分配器。
//!
//! §4 的硬约定：**所有跨边界的内存都由宿主分配、由宿主释放**。插件不许把
//! 自己的 `malloc` 结果交出来，宿主也不许 `free` 一块不是自己发的内存 ——
//! 两个 CRT 实例互相 free 是这类 ABI 最经典的死法。
//!
//! 缓冲区带 4 字节长度前缀，所以 `free(ptr)` 不需要额外参数就能还原布局。

use core::ptr;
use std::alloc::{alloc, dealloc, Layout};

use miy_abi::{buf_total_len, write_buf, MIY_BUF_HDR, MIY_BUF_MAX};

const ALIGN: usize = 8;

/// 分配一块带长度前缀的缓冲区并把 `payload` 写进去。失败返回空指针。
pub fn alloc_buf(payload: &[u8]) -> *mut u8 {
    let total = payload.len() + MIY_BUF_HDR;
    if total > MIY_BUF_MAX + MIY_BUF_HDR {
        return ptr::null_mut();
    }
    let p = alloc_bytes(total);
    if p.is_null() {
        return ptr::null_mut();
    }
    unsafe { write_buf(p, payload) };
    p
}

/// 裸分配（插件一侧 `alloc(size)` 走这里）。
pub fn alloc_bytes(total: usize) -> *mut u8 {
    if total == 0 || total > MIY_BUF_MAX + MIY_BUF_HDR {
        return ptr::null_mut();
    }
    match Layout::from_size_align(total, ALIGN) {
        Ok(layout) => unsafe { alloc(layout) },
        Err(_) => ptr::null_mut(),
    }
}

/// 释放。指针不可信时**宁可泄漏**也不 `dealloc` 一块野内存 —— 一次泄漏
/// 好过一次堆破坏。
pub fn free_buf(p: *mut u8) {
    if p.is_null() {
        return;
    }
    let total = unsafe { buf_total_len(p) };
    if total > MIY_BUF_MAX + MIY_BUF_HDR {
        return;
    }
    if let Ok(layout) = Layout::from_size_align(total, ALIGN) {
        unsafe { dealloc(p, layout) };
    }
}

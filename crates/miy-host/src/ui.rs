//! WebView2 外壳 = splash 先出 + WebView2 异步后到。
//!
//! 顺序是刻意的：spike 实测插件冷加载要 0.2–0.3 s，所以窗口必须在做任何重活之前
//! 就把第一帧画出来，否则"首帧 ≤ 120 ms"这条验收永远过不了。
//!
//! 窗口是**无边框**的：标题栏由页面自己画，三个按钮走 `miy.app.window` 服务
//! 回来。这么做不是为了好看——`WS_OVERLAPPEDWINDOW` 的系统标题栏会和 Material 3
//! 的深色表面打架，且颜色不跟主题走。代价是缩放边、拖动、最大化行为都得自己接。

use std::cell::RefCell;
use std::sync::OnceLock;
use std::time::Instant;

use windows::core::{w, Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DrawTextW, EndPaint, FillRect, GetMonitorInfoW, MonitorFromPoint,
    MonitorFromWindow, SelectObject, SetBkMode, SetTextColor, UpdateWindow, DT_CENTER, DT_SINGLELINE,
    DT_VCENTER, HBRUSH, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, PAINTSTRUCT,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, ReleaseCapture, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetCursorPos, GetMessageW,
    GetSystemMetrics, GetWindowRect, IsZoomed, KillTimer, LoadCursorW, LoadIconW, MessageBoxW,
    PostMessageW,
    PostQuitMessage, RegisterClassW, SendMessageW, SetTimer, ShowWindow, TranslateMessage,
    HTCAPTION, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCLIENT, HTLEFT, HTRIGHT,
    HTTOP, HTTOPLEFT, HTTOPRIGHT, IDC_ARROW, MB_ICONERROR, MB_OK, MINMAXINFO, MSG, SM_CXSCREEN,
    SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOW, WM_APP, WM_CLOSE, WM_DESTROY,
    WM_GETMINMAXINFO, WM_NCACTIVATE, WM_NCCALCSIZE, WM_NCHITTEST, WM_NCLBUTTONDOWN, WM_PAINT,
WM_SIZE, WM_TIMER, WM_ERASEBKGND,
    SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER, WM_DPICHANGED,
    WINDOWPLACEMENT, WNDCLASSW, WINDOW_EX_STYLE, WS_CLIPCHILDREN,
    WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_THICKFRAME, WS_VISIBLE, GetWindowPlacement,
    SetWindowPlacement,
};

use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
};

use crate::{ipc, paths, settings};

// ---- 拖放：把整合包拖进窗口就装 -------------------------------------------
//
// 为什么要在宿主这边接：WebView2 里 `File.path` 永远是 undefined（拿不到真实
// 路径），而安装整合包必须知道路径。所以关掉 WebView2 自己的外部拖放
// （`AllowExternalDrop = false`），在父窗口上注册一个 OLE 拖放目标 —— 子窗口
// 没有目标时 OLE 会沿父链往上找，正好落到我们这里。
//
// 顺带的好处：`DragEnter` / `DragLeave` 能让我们在页面上画那层半透明提示，
// 而 `WM_DROPFILES` 只有落下那一刻才知道。

use windows::core::{implement, Ref};
use windows::Win32::Foundation::POINTL;
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{
    OleInitialize, OleUninitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop, CF_HDROP,
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::EnumChildWindows;

/// 拖放目标。无状态：它只负责把"有东西进来了 / 走了 / 落下了"转成页面事件。
#[implement(IDropTarget)]
struct DropTarget;

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: Ref<IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let ok = !drop_files(pdataobj.as_ref()).is_empty();
        crate::log::debug(&format!("拖放进入：{} 个文件", if ok { 1 } else { 0 }));
        unsafe {
            *effect = if ok { DROPEFFECT_COPY } else { DROPEFFECT_NONE };
        }
        if ok {
            ipc::push_event("drop.enter", serde_json::json!({}));
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe {
            *effect = DROPEFFECT_COPY;
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        ipc::push_event("drop.leave", serde_json::json!({}));
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: Ref<IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let paths = drop_files(pdataobj.as_ref());
        crate::log::debug(&format!("拖放落下：{} 个文件", paths.len()));
        unsafe {
            *effect = if paths.is_empty() {
                DROPEFFECT_NONE
            } else {
                DROPEFFECT_COPY
            };
        }
        ipc::push_event("drop", serde_json::json!({ "paths": paths }));
        Ok(())
    }
}

/// 从拖放数据里取出文件路径（`CF_HDROP`）。
fn drop_files(data: Option<&IDataObject>) -> Vec<String> {
    let Some(data) = data else {
        return Vec::new();
    };
    let fmt = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    unsafe {
        let Ok(medium) = data.GetData(&fmt) else {
            return Vec::new();
        };
        let out = hdrop_paths(HDROP(medium.u.hGlobal.0));
        let mut medium = medium;
        ReleaseStgMedium(&mut medium);
        out
    }
}

/// 从一个 `HDROP` 里读出所有路径。单独拆出来是为了能直接测：
/// 造一个 `DROPFILES` 内存块就能验，不用真的拖一次。
fn hdrop_paths(hdrop: HDROP) -> Vec<String> {
    unsafe {
        let count = DragQueryFileW(hdrop, u32::MAX, None);
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let len = DragQueryFileW(hdrop, i, None);
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let got = DragQueryFileW(hdrop, i, Some(&mut buf));
            if got > 0 {
                out.push(String::from_utf16_lossy(&buf[..got as usize]));
            }
        }
        out
    }
}

const TIMER_DRAIN: usize = 1;

/// 缩放边宽度。WebView2 是独立子窗口，会吞掉自己范围内的鼠标消息，
/// 所以留一条 4 px 的边给父窗口做 `WM_NCHITTEST`——这条边涂成页面同色，
/// 眼睛看不见，但拖边框缩放是真的能用。
const RESIZE_BORDER: i32 = 4;

/// 首帧窗口尺寸（居中显示）。
const WIN_W: i32 = 1120;
const WIN_H: i32 = 720;

// 自定义消息：页面点按钮 → 服务 → 投递到这个窗口的消息队列。
const WM_MIY_DRAG: u32 = WM_APP + 2;
const WM_MIY_MINIMIZE: u32 = WM_APP + 3;
const WM_MIY_TOGGLE_MAX: u32 = WM_APP + 4;
const WM_MIY_CLOSE: u32 = WM_APP + 5;
/// 动画播完了。由 `miy.app.intro` 服务投递过来 —— 换文档只能在 UI 线程做。
const WM_MIY_INTRO_DONE: u32 = WM_APP + 6;

/// 动画的兜底计时器：动画自己没打招呼（脚本挂了、WebView 卡住），也不能让
/// 窗口永远停在片尾。
const TIMER_INTRO: usize = 2;

static WINDOW: OnceLock<isize> = OnceLock::new();

/// 主界面的 HTML。动画先播，界面后到，所以文本得先存着 —— 等动画叫停的时候
/// 再 `NavigateToString` 一次。
static MAIN_HTML: OnceLock<String> = OnceLock::new();

/// 窗口里现在是动画（true）还是界面（false）。只切一次，动画和兜底计时器
/// 同时来也不会互相打架。
static INTRO_LIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 页面的窗口按钮走这里。返回 false 表示窗口还没建好（启动初期会这样）。
pub fn post_action(action: &str) -> bool {
    let Some(raw) = WINDOW.get().copied() else {
        return false;
    };
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    let msg = match action {
        "minimize" => WM_MIY_MINIMIZE,
        "toggle-max" | "maximize" => WM_MIY_TOGGLE_MAX,
        "close" => WM_MIY_CLOSE,
        "drag" => WM_MIY_DRAG,
        _ => return false,
    };
    unsafe { PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(0)).is_ok() }
}

/// 主窗口客户区的物理像素尺寸。
///
/// 页面的「与启动器同尺寸」要拿这个去开游戏，所以取的是客户区而不是整窗：
/// 标题栏和边框不该算进游戏画面。窗口还没建好时返回 (0, 0)。
pub fn client_size() -> (u32, u32) {
    let Some(raw) = WINDOW.get().copied() else {
        return (0, 0);
    };
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    let mut rc = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rc) }.is_err() {
        return (0, 0);
    }
    (
        (rc.right - rc.left).max(0) as u32,
        (rc.bottom - rc.top).max(0) as u32,
    )
}

/// 界面本体。编进 exe（`include_str!`），磁盘上不落一份——单 exe 的承诺靠这个。
fn html() -> &'static str {
    crate::assets::ui()
}

/// 启动动画。和界面一样编进 exe：生成脚本在 `design/intro/build_intro.py`，产物就是
/// 这一份 —— 5 秒、1920x1080，一个方块落下、展开成玻璃、把品牌 M 画在上面，
/// 再收进应用图标。播完自己叫一声，窗口再切到真正的界面；用户看不到任何外部视频文件。
fn intro_html() -> &'static str {
    crate::assets::intro()
}

/// 当前主屏缩放系数（1.0 = 96 DPI）。
///
/// 窗口尺寸要按 DIP 算再乘以它：`WIN_W` 是给页面用的 CSS 像素，150% 的屏上
/// 照样得是 1120 个 CSS 像素宽，否则 WebView 只能用 747 px 去排版，导航栏和
/// 字看着就"过大"了。
fn system_scale() -> f32 {
    let dpi = unsafe { GetDpiForSystem() };
    if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
}

/// 某个窗口所在显示器的缩放系数。
fn window_scale(hwnd: HWND) -> f32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let dpi = if dpi == 0 { unsafe { GetDpiForSystem() } } else { dpi };
    if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
}

/// 主显示器工作区（排除任务栏）。
fn primary_work_area() -> RECT {
    unsafe {
        let hmon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut mi =
            MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            mi.rcWork
        } else {
            RECT {
                left: 0,
                top: 0,
                right: GetSystemMetrics(SM_CXSCREEN),
                bottom: GetSystemMetrics(SM_CYSCREEN),
            }
        }
    }
}

/// 首帧尺寸（物理像素）与工作区。小屏笔记本上 1120×720 DIP 可能放不下，
/// 所以夹到工作区的 92%。
fn initial_size() -> (i32, i32, RECT) {
    let work = primary_work_area();
    let scale = system_scale();
    let max_w = (((work.right - work.left) as f32) * 0.92) as i32;
    let max_h = (((work.bottom - work.top) as f32) * 0.92) as i32;
    let w = ((WIN_W as f32 * scale) as i32).min(max_w).max(520);
    let h = ((WIN_H as f32 * scale) as i32).min(max_h).max(360);
    (w, h, work)
}

/// 把实测数字填进页面。用占位符替换而不是 `format!`，因为页面里的 CSS/JS
/// 本身有大量花括号，`format!` 会把它们当格式串解析。
fn page(first_paint_ms: Option<u128>, ready_ms: u128) -> String {
    let first = first_paint_ms.map_or_else(|| "?".to_string(), |v| v.to_string());
    // 截图脚本要能直接打开某一页，否则每次都得靠人点一下。
    let open_tab = match std::env::var("MIY_UI_TAB") {
        Ok(v) if v.eq_ignore_ascii_case("settings") => "window.__MiyOpenTab='settings';".to_string(),
        // `settings:<owner>` 顺带选中某个插件的设置页。
        Ok(v) if v.starts_with("settings:") => {
            let owner: String = v["settings:".len()..]
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
                .collect();
            format!("window.__MiyOpenTab='settings';window.__MiyOwner='{owner}';")
        }
        Ok(v) if v.eq_ignore_ascii_case("plugins") => "window.__MiyOpenTab='plugins';".to_string(),
        // `page:<plugin>/<id>` 打开某个插件贡献的页面，截图脚本靠它。
        // 允许 `?a=b&c=d`：版本详情页这类页面要用 query 指定对象。
        Ok(v) if v.starts_with("page:") => {
            let key: String = v["page:".len()..]
                .chars()
                .filter(|c| {
                    c.is_ascii_alphanumeric()
                        || matches!(c, '.' | '-' | '_' | '/' | '?' | '=' | '&' | '%' | '+' | ':')
                })
                .collect();
            format!("window.__MiyOpenTab='page:{key}';")
        }
        _ => String::new(),
    };
    html().replace("__FIRST__", &first)
        .replace("__READY__", &ready_ms.to_string())
        .replace("__OPEN_TAB__", &open_tab)
}

static START: OnceLock<Instant> = OnceLock::new();
static FIRST_PAINT_MS: OnceLock<u128> = OnceLock::new();

thread_local! {
    static CONTROLLER: RefCell<Option<ICoreWebView2Controller>> = const { RefCell::new(None) };
}

/// 进程启动时刻（`main` 第一行调用，越早越准）。
pub fn mark_start() {
    let _ = START.set(Instant::now());
}

fn elapsed_ms() -> u128 {
    START.get().map_or(0, |t| t.elapsed().as_millis())
}

/// `--check-webview`：不建窗口、不弹框，只把探测结果打到控制台。
/// 配合 `MIY_FORCE_NO_WEBVIEW=1` 可以验证降级文案。
pub fn check_webview_report() -> i32 {
    match check_runtime() {
        Ok(()) => {
            println!("[ui] OK  WebView2 可用");
            0
        }
        Err(msg) => {
            println!("[ui] MISSING  降级提示文案如下：\n----\n{msg}\n----");
            2
        }
    }
}

/// 窗口几何记忆文件。跟 `settings.json` 分开放：那是用户可改的配置，这是
/// 系统自己维护的状态，混在一起会让"恢复默认"顺手把窗口尺寸也重置掉。
fn placement_file() -> std::path::PathBuf {
    paths::data_root().join("window.json")
}

/// 上次的窗口位置/大小/最大化状态。用 `WINDOWPLACEMENT` 而不是手记矩形：
/// 它自带"正常状态"的坐标，最小化时保存也不会把窗口记到屏幕外的 -32000。
fn load_placement() -> Option<WINDOWPLACEMENT> {
    let text = std::fs::read_to_string(placement_file()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let num = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0) as i32;
    let (l, t) = (num("left"), num("top"));
    let (r, b) = (num("right"), num("bottom"));
    if r - l < 640 || b - t < 480 {
        return None; // 尺寸不靠谱就当没记过
    }
    // 换了显示器（或拔了副屏）时，别把窗口还原到看不见的地方。
    let sx = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let sy = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let sw = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let sh = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    let visible = l < sx + sw && r > sx && t < sy + sh && b > sy;
    if !visible {
        return None;
    }
    let mut p =
        WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    p.showCmd = num("showCmd").clamp(1, 3) as u32; // 1 正常 / 2 最小化 / 3 最大化
    if p.showCmd == 2 {
        // 上次是"最小化着关掉的"。还原成最小化会让用户双击图标后什么也看不到，
        // 所以按正常状态开。
        p.showCmd = 1;
    }
    // 记下来的是物理像素，窗口却可能开在另一块缩放不同的屏上。按两次缩放
    // 的比值围绕中心缩放一遍，尺寸才不会在 100% ↔ 200% 之间跳来跳去。
    let saved_dpi = v.get("dpi").and_then(|x| x.as_i64()).unwrap_or(96).max(72) as f32;
    let now_dpi = unsafe { GetDpiForSystem() }.max(72) as f32;
    let k = now_dpi / saved_dpi;
    let (mut l, mut t, mut r, mut b) = (l as f32, t as f32, r as f32, b as f32);
    if (k - 1.0).abs() > 0.01 {
        let (cx, cy) = ((l + r) / 2.0, (t + b) / 2.0);
        let (w, h) = ((r - l) * k, (b - t) * k);
        l = cx - w / 2.0;
        t = cy - h / 2.0;
        r = cx + w / 2.0;
        b = cy + h / 2.0;
    }
    p.rcNormalPosition = RECT {
        left: l as i32,
        top: t as i32,
        right: r as i32,
        bottom: b as i32,
    };
    Some(p)
}

/// 关窗时记一次。失败就失败：位置记不住不影响用。
fn save_placement(hwnd: HWND) {
    unsafe {
        let mut p = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        if GetWindowPlacement(hwnd, &mut p).is_err() {
            return;
        }
        let r = p.rcNormalPosition;
        // 最小化状态不记：从任务栏"最小化→右键关闭"是最常见的关法之一，
        // 记下来就成了"启动后看不见窗口"。
        let show = if p.showCmd == 2 { 1 } else { p.showCmd };
        let text = serde_json::json!({
            "left": r.left,
            "top": r.top,
            "right": r.right,
            "bottom": r.bottom,
            "showCmd": show,
            // 存物理像素就必须连缩放一起存，否则换台机器或改缩放后还原出来的
            // 窗口会明显偏大/偏小。
            "dpi": GetDpiForWindow(hwnd).max(72),
        })
        .to_string();
        let _ = std::fs::write(placement_file(), text);
    }
}

/// 起不来的时候（比如程序目录不可写），至少用系统对话框把原因说清楚 ——
/// 这时候还没有 WebView2 可用，界面是打不开的。
pub fn fatal(why: &str) {
    unsafe {
        MessageBoxW(
            None,
            &windows::core::HSTRING::from(why),
            w!("MIYLauncher"),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// 动画播完了（页面调 `miy.app.intro` 通知）。只是投一条窗口消息：换文档必须
/// 回到 UI 线程做，而这条消息是从 IPC 线程来的。
pub fn intro_done() {
    let Some(raw) = WINDOW.get().copied() else {
        return;
    };
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_MIY_INTRO_DONE, WPARAM(0), LPARAM(0));
    }
}

/// 把主界面换上来。只在 UI 线程（窗口过程）里调用。
fn show_main(hwnd: HWND) {
    // 只切一次：动画的 `miy.app.intro` 和兜底计时器可能前后脚都来。
    if !INTRO_LIVE.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let _ = unsafe { KillTimer(Some(hwnd), TIMER_INTRO) };
    let Some(html) = MAIN_HTML.get() else {
        return;
    };
    CONTROLLER.with(|c| {
        if let Some(ctrl) = c.borrow().as_ref() {
            if let Ok(webview) = unsafe { ctrl.CoreWebView2() } {
                let h = windows::core::HSTRING::from(html.as_str());
                ipc::allow_next_navigation();
                let _ = unsafe { webview.NavigateToString(&h) };
                println!("[ui] 动画结束 {} ms，切换到主界面", elapsed_ms());
            }
        }
    });
}

pub fn run(probe: bool) -> i32 {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        // OLE 拖放要求线程先初始化 OLE（`RegisterDragDrop` 会失败在没初始化的线程上）。
        let _ = OleInitialize(None);
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // 先确认运行时在不在 —— 不在就别开窗，直接说人话。
    if let Err(why) = check_runtime() {
        unsafe {
            MessageBoxW(
                None,
                &windows::core::HSTRING::from(why.as_str()),
                w!("MIYLauncher"),
                MB_OK | MB_ICONERROR,
            );
        }
        return 2;
    }

    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("MIYLauncherWindow");
        // 现在的外壳是 M3 浅色，窗口背景跟着走（不然缩放时会闪出一圈深色）。
        let class_bg = COLORREF(0x00faf9f9);
        let brush = CreateSolidBrush(class_bg);
        // 图标由 build.rs 用 rc.exe 编进 exe（资源 id 1）。取不到就用系统默认，
        // 不要因为一个图标让启动失败。
        let app_icon = LoadIconW(Some(HINSTANCE(hinstance.0)), PCWSTR(1 as *const u16))
            .unwrap_or_default();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: HINSTANCE(hinstance.0),
            lpszClassName: class,
            hIcon: app_icon,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: brush,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return 3;
        }

        // 无边框但保留可缩放：`WS_POPUP` 去掉标题栏，`WS_THICKFRAME` 留阴影与缩放。
        // 只要 `WM_NCCALCSIZE` 返回 0，客户区就等于整个窗口。
        let style = WS_POPUP
            | WS_THICKFRAME
            | WS_MINIMIZEBOX
            | WS_MAXIMIZEBOX
            | WS_CLIPCHILDREN
            | WS_VISIBLE;
        let (ww, wh, work) = initial_size();
        let sw = work.right - work.left;
        let sh = work.bottom - work.top;
        let x = work.left + ((sw - ww) / 2).max(0);
        let y = work.top + ((sh - wh) / 2).max(0);

        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("MIYLauncher"),
            style,
            x,
            y,
            ww,
            wh,
            None,
            None,
            Some(HINSTANCE(hinstance.0)),
            None,
        ) {
            Ok(h) => h,
            Err(_) => return 4,
        };
        let _ = WINDOW.set(hwnd.0 as isize);

        // 拖放目标先挂在父窗口上（窗口边缘那圈 4 px 是父窗口的），
        // WebView2 起来之后再挂到它的子窗口上（见 start_webview）。
        let drop_target: IDropTarget = DropTarget.into();
        if let Err(e) = RegisterDragDrop(hwnd, Some(&drop_target)) {
            crate::log::warn(&format!("注册拖放目标失败：{e}"));
        }

        // 有记过位置就还原，没有就用居中那套。
        if let Some(p) = load_placement() {
            let _ = SetWindowPlacement(hwnd, &p);
        }

        // 标题栏是自绘的，DWM 的深色/浅色边框只影响阴影与描边：浅色主题要关掉。
        let dark: i32 = 0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const core::ffi::c_void,
            4,
        );
        let round: i32 = 2; // DWMWCP_ROUND
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &round as *const _ as *const core::ffi::c_void,
            4,
        );
        // 1 px 描边是 DWM 画的，不是我们画的：窗口一失去焦点，Win11 就沿着
        // 四边描一圈浅色，在深色界面上就是一条刺眼的白边（点"打开目录"把
        // 资源管理器叫到前台再回来，每次都看得见）。直接让它别画。
        let none: u32 = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &none as *const _ as *const core::ffi::c_void,
            4,
        );

        // 这两句是"首帧"的真正来源：ShowWindow 触发 WM_PAINT，UpdateWindow 把
        // 重绘同步做完并返回 —— 之后的代码再慢也不影响用户已经看到画面。
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);

        // 兜底：即使某条路径漏了 PostMessage，事件也不会一直躺在队列里。
        let _ = SetTimer(Some(hwnd), TIMER_DRAIN, 100, None);

        // 启动动画：只在正常启动时放。探针、截图脚本、以及用户明确关掉的时候
        // 直接进界面 —— 那些场景等 5 秒纯属浪费。
        let intro = !probe
            && std::env::var_os("MIY_NO_INTRO").is_none()
            && std::env::var_os("MIY_UI_TAB").is_none()
            && settings::effective_str("core.ui.intro").as_deref() != Some("false");
        start_webview(hwnd, intro, drop_target.clone());

        // `--ui-probe`：跑 2.5 s 后自己关窗，让首帧指标可以无人值守地量出来。
        if probe {
            let hwnd_bits = hwnd.0 as usize;
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(2500));
                let h = HWND(hwnd_bits as *mut core::ffi::c_void);
                let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
            });
        }

        // `--drop-test <路径>`：把"拖进来"这件事在宿主侧重放一遍（提示层 → 安装），
        // 用来验收拖放链路。真实拖放走的是同一个事件通道，只是入口换成 OLE。
        if let Some(path) = crate::arg_value("--drop-test") {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(2500));
                ipc::push_event("drop.enter", serde_json::json!({}));
                std::thread::sleep(std::time::Duration::from_millis(900));
                ipc::push_event("drop", serde_json::json!({ "paths": [path] }));
            });
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = RevokeDragDrop(hwnd);
        OleUninitialize();
        0
    }
}

fn runtime_hint() -> String {    "缺少 Microsoft Edge WebView2 运行时。\n\n\
     启动器界面依赖该组件。Windows 10 21H2 及更高版本、Windows 11 已默认预装，旧系统需要手动安装一次：\n\
     https://developer.microsoft.com/microsoft-edge/webview2/\n\n\
     安装完成后重新启动启动器即可。"
        .to_string()
}

/// 运行时探测：明确区分"没装 WebView2"与"装了但坏"，两种给不同的处置建议。
fn check_runtime() -> Result<(), String> {
    // 用来验证降级路径（真实环境里没法把 WebView2 卸掉再测）。
    if std::env::var_os("MIY_FORCE_NO_WEBVIEW").is_some() {
        return Err(runtime_hint());
    }

    let mut version = PWSTR::null();
    let hr = unsafe { GetAvailableCoreWebView2BrowserVersionString(PCWSTR::null(), &mut version) };
    if let Err(e) = hr {
        return Err(format!("{e}\n\n{}", runtime_hint()));
    }
    let text = unsafe { version.to_string().unwrap_or_default() };
    unsafe { CoTaskMemFree(Some(version.0 as *const _)) };
    println!("[ui] WebView2 运行时 {text}");
    Ok(())
}

/// WebView2 的可见区域：客户区往里收 [`RESIZE_BORDER`]，把最外圈让给父窗口做缩放边。
fn apply_bounds(hwnd: HWND) {
    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut rc) };
    let b = (RESIZE_BORDER as f32 * window_scale(hwnd)).round() as i32;
    let view = RECT {
        left: rc.left + b,
        top: rc.top + b,
        right: rc.right - b,
        bottom: rc.bottom - b,
    };
    CONTROLLER.with(|c| {
        if let Some(ctrl) = c.borrow().as_ref() {
            let _ = unsafe { ctrl.SetBounds(view) };
        }
    });
}

/// 把拖放目标挂到 `root` 及其所有子孙窗口上，返回挂成功的个数。
///
/// 为什么要挂到子窗口：OLE 的 `DoDragDrop` 从鼠标下的窗口开始找目标，
/// 命中 WebView2 自己的窗口就停了，不会继续往父窗口找。WebView2 的窗口树
/// 是 `Chrome_WidgetWin_0 → Chrome_WidgetWin_1 → Chrome_RenderWidgetHostHWND`，
/// 逐个换掉最省事。
fn attach_drop_to_tree(root: HWND, target: &IDropTarget) -> usize {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let list = &mut *(lparam.0 as *mut Vec<HWND>);
        list.push(hwnd);
        windows::core::BOOL(1)
    }

    let mut list: Vec<HWND> = vec![root];
    unsafe {
        let _ = EnumChildWindows(
            Some(root),
            Some(collect),
            LPARAM(&mut list as *mut Vec<HWND> as isize),
        );
    }

    let mut n = 0;
    for h in list {
        unsafe {
            let _ = RevokeDragDrop(h);
            if RegisterDragDrop(h, Some(target)).is_ok() {
                n += 1;
            }
        }
    }
    n
}

fn start_webview(hwnd: HWND, intro: bool, drop_target: IDropTarget) {
    // 用户数据目录必须在可写位置：装到 Program Files 时 exe 旁边写不了。
    let user_data = paths::webview_dir();
    if let Err(e) = std::fs::create_dir_all(&user_data) {
        eprintln!("[ui] 创建 WebView2 数据目录失败: {e}");
    }
    let user_data_wide = windows::core::HSTRING::from(user_data.to_string_lossy().as_ref());

    let hwnd_raw = hwnd.0;

    unsafe {
        let env_handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
            move |hr, env: Option<ICoreWebView2Environment>| {
                if let Err(e) = hr {
                    eprintln!("[ui] 环境创建失败: {e}");
                    crate::bus::get()
                        .emit("host.error", format!("WebView2 环境创建失败：{e}").as_bytes());
                    return Ok(());
                }
                let Some(env) = env else { return Ok(()) };
                println!("[ui] 环境就绪 {} ms", elapsed_ms());

                let ctrl_handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
                    move |hr, ctrl: Option<ICoreWebView2Controller>| {
                        if let Err(e) = hr {
                            eprintln!("[ui] 控制器创建失败: {e}");
                            return Ok(());
                        }
                        let Some(ctrl) = ctrl else { return Ok(()) };
                        // 先入列再量尺寸：apply_bounds 是从 thread_local 里取控制器的，
                        // 顺序反了就会对着空气设区域 —— 表现是窗口永远停在 splash 上。
                        CONTROLLER.with(|c| *c.borrow_mut() = Some(ctrl.clone()));
                        let _ = ctrl.SetIsVisible(true);
                        apply_bounds(HWND(hwnd_raw));
                        match ctrl.CoreWebView2() {
                            Ok(webview) => {
                                // 这是一个启动器窗口，不是浏览器：右键菜单、状态栏气泡、
                                // F5/Ctrl+P 之类的浏览器快捷键都不该出现在这里。
                                // 开发版留 DevTools，正式版关掉，免得用户误开调试面板。
                                if let Ok(s) = webview.Settings() {
                                    let _ = s.SetAreDefaultContextMenusEnabled(false);
                                    let _ = s.SetIsStatusBarEnabled(false);
                                    let _ = s.SetIsZoomControlEnabled(false);
                                    let _ = s.SetAreDevToolsEnabled(cfg!(debug_assertions));
                                    if let Ok(s3) =
                                        s.cast::<ICoreWebView2Settings3>()
                                    {
                                        let _ = s3.SetAreBrowserAcceleratorKeysEnabled(false);
                                    }
                                }
                                // 拖进来的文件交给宿主处理（见文件头的 DropTarget）：
                                // 开着的话 WebView2 会把它当导航/下载，正是用户抱怨的那个行为。
                                if let Ok(c4) = ctrl.cast::<ICoreWebView2Controller4>() {
                                    let _ = c4.SetAllowExternalDrop(false);
                                }
                                // 光关掉还不够：WebView2 的窗口树里可能已经有自己的拖放
                                // 目标，OLE 命中它就不会再往父窗口找 —— 表现就是拖到窗口上
                                // 只有一个"禁止"光标。把整棵子窗口树的目标都换成我们的。
                                let n = attach_drop_to_tree(HWND(hwnd_raw), &drop_target);
                                crate::log::debug(&format!("拖放目标已挂到 {n} 个窗口"));
                                // 桥必须在导航前挂好：脚本注入是针对"之后创建的文档"。
                                if let Err(e) = ipc::attach(&webview, HWND(hwnd_raw)) {
                                    eprintln!("[ui] 桥接挂载失败: {e}");
                                }
                                let ready = elapsed_ms();
                                let first = FIRST_PAINT_MS.get().copied();
                                let html = page(first, ready);
                                // 界面文本先存好：动画播完才用它，而那时这里已经出了作用域。
                                let _ = MAIN_HTML.set(html.clone());
                                if intro {
                                    // 动画里没有需要替换的占位符，原样送进去。
                                    INTRO_LIVE.store(true, std::sync::atomic::Ordering::SeqCst);
                                    let h = windows::core::HSTRING::from(intro_html());
                                                        ipc::allow_next_navigation();
                                                        let _ = webview.NavigateToString(&h);
                                    // 兜底：动画最长 5 s，加满启动开销也不该超过 9 s。
                                    let _ = SetTimer(Some(HWND(hwnd_raw)), TIMER_INTRO, 9000, None);
                                } else {
                                    let h = windows::core::HSTRING::from(html.as_str());
                                                        ipc::allow_next_navigation();
                                                        let _ = webview.NavigateToString(&h);
                                }
                                let first_txt =
                                    first.map_or_else(|| "?".to_string(), |v| format!("{v} ms"));
                                println!(
                                    "[ui] WebView2 就绪 {ready} ms（首帧 {first_txt}{}）",
                                    if intro { "，先播启动动画" } else { "" }
                                );
                            }
                            Err(e) => eprintln!("[ui] CoreWebView2 拿不到: {e}"),
                        }
                        Ok(())
                    },
                ));

                if let Err(e) = env.CreateCoreWebView2Controller(HWND(hwnd_raw), &ctrl_handler) {
                    eprintln!("[ui] CreateCoreWebView2Controller 失败: {e}");
                }
                Ok(())
            },
        ));

        if let Err(e) = CreateCoreWebView2EnvironmentWithOptions(
            PCWSTR::null(),
            PCWSTR::from_raw(user_data_wide.as_ptr()),
            None,
            &env_handler,
        ) {
            eprintln!("[ui] CreateCoreWebView2EnvironmentWithOptions 失败: {e}");
        }
    }
}

/// 无边框窗口的缩放边判定。窗口矩形与 `lparam` 都是屏幕坐标。
unsafe fn hit_test(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    // 最大化时没有可拖的边，返回 HTCLIENT 让页面自己收事件。
    if IsZoomed(hwnd).as_bool() {
        return LRESULT(HTCLIENT as isize);
    }
    let x = (lparam.0 & 0xFFFF) as i16 as i32;
    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
    let mut rc = RECT::default();
    if GetWindowRect(hwnd, &mut rc).is_err() {
        return LRESULT(HTCLIENT as isize);
    }
    // 缩放边也要跟着 DPI 放大：200% 屏上 4 物理像素只有 2 个 CSS 像素宽，
    // 手根本点不准。
    let b = (RESIZE_BORDER as f32 * window_scale(hwnd)).round() as i32;
    let (left, right) = (x < rc.left + b, x >= rc.right - b);
    let (top, bottom) = (y < rc.top + b, y >= rc.bottom - b);
    let ht = match (left, right, top, bottom) {
        (true, _, true, _) => HTTOPLEFT,
        (_, true, true, _) => HTTOPRIGHT,
        (true, _, _, true) => HTBOTTOMLEFT,
        (_, true, _, true) => HTBOTTOMRIGHT,
        (true, _, _, _) => HTLEFT,
        (_, true, _, _) => HTRIGHT,
        (_, _, true, _) => HTTOP,
        (_, _, _, true) => HTBOTTOM,
        _ => HTCLIENT,
    };
    LRESULT(ht as isize)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    {
        match msg {
            WM_NCCALCSIZE if wparam.0 != 0 => {
                // 客户区 = 整个窗口。这一句就是"没有标题栏"的全部实现。
                LRESULT(0)
            }
            WM_NCHITTEST => hit_test(hwnd, lparam),
            WM_MIY_DRAG => {
                // 在 UI 线程上发起非客户区拖动：它会进入系统自己的模态拖动循环，
                // Aero Snap、双击最大化、拖到顶边最大化全都免费拿到。
                //
                // 两个前提缺一不可：
                // 1. 鼠标左键还按着。页面是异步把消息传回来的，用户点一下就走的话
                //    拖动循环起来后又立刻结束，看起来就是"拖不动"。
                // 2. 先释放鼠标捕获。按下的动作发生在 WebView2 子窗口里，捕获在它
                //    手上，不放开的话拖动循环拿不到鼠标移动消息。
                if GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000 == 0 {
                    return LRESULT(0);
                }
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ReleaseCapture();
                let lp = (((pt.y as isize) << 16) | (pt.x as isize & 0xFFFF)) as isize;
                SendMessageW(
                    hwnd,
                    WM_NCLBUTTONDOWN,
                    Some(WPARAM(HTCAPTION as usize)),
                    Some(LPARAM(lp)),
                );
                LRESULT(0)
            }
            // 拖到另一块缩放不同的显示器上：系统给的建议矩形已经是按新 DPI 折算
            // 好的，照做即可；不接这条消息的话窗口会保持旧物理尺寸，在新屏上就
            // 显得又小又挤。
            WM_DPICHANGED => {
                let suggested = lparam.0 as *const RECT;
                if !suggested.is_null() {
                    let r = *suggested;
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    apply_bounds(hwnd);
                }
                LRESULT(0)
            }
            WM_MIY_MINIMIZE => {
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
                LRESULT(0)
            }
            WM_MIY_TOGGLE_MAX => {
                let cmd = if IsZoomed(hwnd).as_bool() {
                    SW_RESTORE
                } else {
                    SW_MAXIMIZE
                };
                let _ = ShowWindow(hwnd, cmd);
                LRESULT(0)
            }
            WM_MIY_CLOSE => {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                LRESULT(0)
            }
            // WS_POPUP + WS_THICKFRAME 的组合下，系统按"边框可见"来算最大化尺寸，
            // 结果窗口比屏幕宽出几像素、四条边都跑到屏幕外（实测 2574x1454 / 屏
            // 2560x1440）。自己按显示器工作区给上限。
            WM_GETMINMAXINFO => {
                let mmi = lparam.0 as *mut MINMAXINFO;
                if !mmi.is_null() {
                    let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                    let mut mi = MONITORINFO {
                        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                        ..Default::default()
                    };
                    if GetMonitorInfoW(hmon, &mut mi).as_bool() {
                        let r = mi.rcWork;
                        (*mmi).ptMaxPosition.x = r.left;
                        (*mmi).ptMaxPosition.y = r.top;
                        (*mmi).ptMaxSize.x = r.right - r.left;
                        (*mmi).ptMaxSize.y = r.bottom - r.top;
                    }
                    // 最小尺寸也按 DIP 给：不留这个，用户能把窗口拖到 300 px 宽，
                    // 侧栏和导航挤成一团，看着就像"界面坏了"。
                    let scale = window_scale(hwnd);
                    (*mmi).ptMinTrackSize.x = (760.0 * scale) as i32;
                    (*mmi).ptMinTrackSize.y = (520.0 * scale) as i32;
                }
                LRESULT(0)
            }
            // 整个客户区都是我们画的，别让系统先拿白刷子擦一遍（拖动边框时会闪）。
            WM_ERASEBKGND => LRESULT(1),
            // 没有非客户区，激活状态变化不需要系统重画任何东西。
            WM_NCACTIVATE => LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let brush: HBRUSH = CreateSolidBrush(COLORREF(0x00faf9f9));
                FillRect(hdc, &rc, brush);

                SetBkMode(hdc, windows::Win32::Graphics::Gdi::TRANSPARENT);
                SetTextColor(hdc, COLORREF(0x00201c19));
                let font = windows::Win32::Graphics::Gdi::GetStockObject(
                    windows::Win32::Graphics::Gdi::DEFAULT_GUI_FONT,
                );
                SelectObject(hdc, font);

                let mut title: Vec<u16> = "MIYLauncher".encode_utf16().collect();
                // 行高按缩放放大：高 DPI 屏上还按 40/30 物理像素摆，两行字会挤在
                // 屏幕正中间一小块里。
                let scale = window_scale(hwnd);
                let gap = (40.0 * scale) as i32;
                let mut r1 = RECT {
                    top: rc.bottom / 2 - gap,
                    bottom: rc.bottom / 2,
                    ..rc
                };
                DrawTextW(
                    hdc,
                    &mut title,
                    &mut r1,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );

                let mut sub: Vec<u16> = "正在启动…".encode_utf16().collect();
                let mut r2 = RECT {
                    top: rc.bottom / 2,
                    bottom: rc.bottom / 2 + (30.0 * scale) as i32,
                    ..rc
                };
                SetTextColor(hdc, COLORREF(0x004e4744));
                DrawTextW(hdc, &mut sub, &mut r2, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

                let _ = EndPaint(hwnd, &ps);

                // 第一帧只记一次。
                let ms = elapsed_ms();
                if FIRST_PAINT_MS.set(ms).is_ok() {
                    println!("[ui] splash 首帧 {ms} ms");
                }
                LRESULT(0)
            }
            ipc::WM_BRIDGE => {
                ipc::flush_events();
                ipc::drain();
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_DRAIN => {
                ipc::flush_events();
                ipc::drain();
                LRESULT(0)
            }
            WM_MIY_INTRO_DONE => {
                show_main(hwnd);
                LRESULT(0)
            }
            // 动画没打招呼就结束了（脚本异常、WebView 卡死）。宁可硬切，也别让
            // 用户对着片尾的一个方块干等。
            WM_TIMER if wparam.0 == TIMER_INTRO => {
                show_main(hwnd);
                LRESULT(0)
            }
            WM_SIZE => {
                apply_bounds(hwnd);
                let maxed = IsZoomed(hwnd).as_bool();
                crate::bus::get().emit(
                    "window.state",
                    format!("{{\"maximized\":{maxed}}}").as_bytes(),
                );
                LRESULT(0)
            }
            WM_DESTROY => {
                save_placement(hwnd);
                let _ = KillTimer(Some(hwnd), TIMER_DRAIN);
                let _ = KillTimer(Some(hwnd), TIMER_INTRO);
                CONTROLLER.with(|c| {
                    if let Some(ctrl) = c.borrow().as_ref() {
                        let _ = ctrl.Close();
                    }
                });
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod drop_tests {
    use super::*;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Foundation::GlobalFree;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    /// `DROPFILES` 头（20 字节）：偏移、鼠标位置、是否非客户区、是否宽字符。
    #[repr(C)]
    struct DropFiles {
        p_files: u32,
        pt: POINT,
        f_nc: windows::core::BOOL,
        f_wide: windows::core::BOOL,
    }

    #[test]
    fn reads_paths_from_hdrop() {
        let paths = ["C:\\a\\b.mrpack", "D:\\中文 目录\\c.zip"];
        let mut wide: Vec<u16> = Vec::new();
        for p in paths {
            wide.extend(p.encode_utf16());
            wide.push(0);
        }
        wide.push(0);

        let head = std::mem::size_of::<DropFiles>();
        unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE, head + wide.len() * 2).expect("alloc");
            let base = GlobalLock(h) as *mut u8;
            let df = DropFiles {
                p_files: head as u32,
                pt: POINT { x: 0, y: 0 },
                f_nc: false.into(),
                f_wide: true.into(),
            };
            std::ptr::copy_nonoverlapping(&df as *const _ as *const u8, base, head);
            std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, base.add(head), wide.len() * 2);
            let _ = GlobalUnlock(h);

            let got = hdrop_paths(HDROP(h.0));
            let want: Vec<String> = paths.iter().map(|s| s.to_string()).collect();
            assert_eq!(got, want);

            let _ = GlobalFree(Some(h));
        }
    }
}

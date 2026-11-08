//! WebView2 ↔ 总线 的桥。
//!
//! 线程模型是这里唯一重要的事：
//!
//! * `WebMessageReceived` 在 **UI 线程**（STA）触发。
//! * 真正的派发必须离开 UI 线程 —— 否则一个慢插件就能让窗口"未响应"。
//! * WebView2 的接口**只能在 UI 线程**调，所以结果要绕回 UI 线程再
//!   `PostWebMessageAsJson`。
//!
//! 于是数据流是：JS →(UI线程)→ 丢给线程 → 总线派发 → 回队列 →(UI线程)→ JS。
//!
//! JS 侧用法（见 `INIT_JS`）：
//! ```js
//! const pong = await miy.call('miy.hello.ping', {});
//! miy.onEvent(({topic, payload}) => console.log(topic, payload));
//! ```

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2, ICoreWebView2NavigationStartingEventArgs, ICoreWebView2WebMessageReceivedEventArgs,
};
use webview2_com::{
    take_pwstr, AddScriptToExecuteOnDocumentCreatedCompletedHandler, NavigationStartingEventHandler,
    WebMessageReceivedEventHandler,
};
use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::{bus, log};

/// UI 线程收到这条消息就去把队列里的东西推给 WebView。
pub const WM_BRIDGE: u32 = WM_APP + 1;

thread_local! {
    static WEBVIEW: RefCell<Option<ICoreWebView2>> = const { RefCell::new(None) };
}

static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
static OUTBOX: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// 我们还欠 WebView 几次页面跳转。
///
/// 界面只有两种"合法"导航：开屏动画和主界面，都是我们自己 `NavigateToString` 送的。
/// 其余一律拦下来 —— 鼠标侧键（后退/前进）、Alt+方向键、拖进窗口的链接、页面里的
/// `history.back()`，都会走同一条 `NavigationStarting`，不拦就会把界面换成一片空白
/// 或者倒回动画文档（那页不会再喊"动画结束"，就卡住了）。
static ALLOW_NAV: AtomicUsize = AtomicUsize::new(0);

/// 已经往这个 WebView 里送过几次文档。`about:blank` 只在"还没送过任何文档"时放行：
/// 那是控制器自己建空文档的那一次，放行了才不会把第一份界面误拦成白屏。
static DOCS_SENT: AtomicUsize = AtomicUsize::new(0);

/// 调 `NavigateToString` 之前先记一笔，让下一次导航放行。
pub fn allow_next_navigation() {
    DOCS_SENT.fetch_add(1, Ordering::SeqCst);
    ALLOW_NAV.fetch_add(1, Ordering::SeqCst);
}

/// 日志里只留开头 —— `NavigateToString` 的 URI 是一整页 base64。
fn preview(uri: &str) -> String {
    let tail = if uri.len() > 64 { "…" } else { "" };
    let cut: String = uri.chars().take(64).collect();
    format!("{cut}{tail}")
}

/// 注入到每个文档里的 JS 胶水。
const INIT_JS: &str = r#"
(function () {
  if (window.miy) return;
  const pending = new Map();
  const early = [];
  let handler = null;
  let seq = 1;
  chrome.webview.addEventListener('message', (e) => {
    const m = e.data;
    if (!m) return;
    // 事件可能比页面自己的 onEvent 注册还早到（插件加载就是），先存着。
    if (m.type === 'event') {
      if (handler) handler(m); else early.push(m);
      return;
    }
    const p = pending.get(m.id);
    if (!p) return;
    pending.delete(m.id);
    if (m.ok) p.resolve(m.result);
    else p.reject(Object.assign(new Error('miy:' + m.code), { code: m.code }));
  });
  window.miy = {
    call(service, req, timeoutMs) {
      const id = seq++;
      const ms = Math.max(1000, Number(timeoutMs) || 20000);
      return new Promise((resolve, reject) => {
        // 宿主那边要是没回话（服务 panic、线程没了、消息丢了），
        // 这里必须自己收尾 —— 否则页面会一直停在"正在加载"。
        const timer = setTimeout(() => {
          if (!pending.has(id)) return;
          pending.delete(id);
          reject(Object.assign(new Error('miy:timeout'), { code: 'timeout' }));
        }, ms + 2000);
        pending.set(id, {
          resolve: v => { clearTimeout(timer); resolve(v); },
          reject: e => { clearTimeout(timer); reject(e); },
        });
        chrome.webview.postMessage(JSON.stringify({
          id, service, req: req === undefined ? null : req, timeout: ms
        }));
      });
    },
    onEvent(fn) {
      handler = fn;
      while (early.length) fn(early.shift());
    }
  };
  window.dispatchEvent(new Event('miy-ready'));
})();
"#;

/// 挂上桥。`hwnd` 用来把结果从工作线程唤醒回 UI 线程。
pub fn attach(webview: &ICoreWebView2, hwnd: HWND) -> windows::core::Result<()> {
    HWND_RAW.store(hwnd.0 as isize, Ordering::Relaxed);
    WEBVIEW.with(|w| *w.borrow_mut() = Some(webview.clone()));
    // 事件唤醒：任何线程 emit 之后 PostMessage 一次，UI 线程来取
    let h = hwnd.0 as isize;
    bus::get().set_waker(move || unsafe {
        let _ = PostMessageW(Some(HWND(h as *mut _)), WM_BRIDGE, WPARAM(0), LPARAM(0));
    });

    let script_handler = AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new(
        |hr, _id: String| {
            if let Err(e) = hr {
                log::warn(&format!("注入桥接脚本失败: {e}"));
            }
            Ok(())
        },
    ));
    unsafe { webview.AddScriptToExecuteOnDocumentCreated(&HSTRING::from(INIT_JS), &script_handler)? };

    let msg_handler = WebMessageReceivedEventHandler::create(Box::new(
        |_sender, args: Option<ICoreWebView2WebMessageReceivedEventArgs>| {
            if let Some(args) = args {
                let mut raw = PWSTR::null();
                if unsafe { args.TryGetWebMessageAsString(&mut raw) }.is_ok() {
                    let text = take_pwstr(raw);
                    handle_incoming(&text);
                }
            }
            Ok(())
        },
    ));
    let mut token = 0i64;
    unsafe { webview.add_WebMessageReceived(&msg_handler, &mut token)? };

    // 导航闸门：每次 `NavigateToString` 之前先记一笔，只有这一笔对应的导航放行。
    //
    // 侧键的后退/前进、Alt+←、点链接、页面里的 `history.back()` 都会走同一条
    // `NavigationStarting`，而且实测 `IsUserInitiated` 对它并不总是 true，光看这个
    // 标记挡不住；所以改成"只放行我们自己刚刚发起的那几次"。
    //
    // `about:blank` 只在还没送过任何文档时放行 —— 那是控制器建空文档的那一次，
    // 放行后才不会把第一份界面误拦成白屏；之后再退到空白页照样拦下。
    let nav_handler = NavigationStartingEventHandler::create(Box::new(
        |_sender, args: Option<ICoreWebView2NavigationStartingEventArgs>| {
            if let Some(args) = args {
                let mut uri = PWSTR::null();
                let target = if unsafe { args.Uri(&mut uri) }.is_ok() {
                    take_pwstr(uri)
                } else {
                    String::new()
                };
                if target.starts_with("about:blank") && DOCS_SENT.load(Ordering::SeqCst) == 0 {
                    return Ok(());
                }
                let ours = ALLOW_NAV
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok();
                if !ours {
                    log::warn(&format!("拦下一次页面跳转（{}）", preview(&target)));
                    let _ = unsafe { args.SetCancel(true) };
                }
            }
            Ok(())
        },
    ));
    let mut nav_token = 0i64;
    unsafe { webview.add_NavigationStarting(&nav_handler, &mut nav_token)? };
    Ok(())
}

/// JS 发来一条消息。**注意这是在 UI 线程上**，所以只做解析 + 转交。
fn handle_incoming(text: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        log::warn(&format!("桥接消息不是 JSON: {text}"));
        return;
    };
    let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
    let Some(service) = v.get("service").and_then(|x| x.as_str()).map(str::to_owned) else {
        return;
    };
    let req = v
        .get("req")
        .map(|r| r.to_string().into_bytes())
        .unwrap_or_default();
    let timeout = v
        .get("timeout")
        .and_then(|x| x.as_u64())
        .map_or(bus::DEFAULT_TIMEOUT, |ms| Duration::from_millis(ms.clamp(100, 600_000)));

    // 每次调用一个线程：这里都是"用户手动触发"级别的频率，池子留给插件用，
    // 免得嵌套调用把池子堵死。
    let _ = std::thread::Builder::new()
        .name(format!("miy-ipc-{id}"))
        .spawn(move || {
            let msg = match bus::get().call(&service, &req, timeout) {
                Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
                    Ok(value) => serde_json::json!({ "id": id, "ok": true, "result": value }),
                    Err(_) => serde_json::json!({
                        "id": id,
                        "ok": true,
                        "result": String::from_utf8_lossy(&bytes),
                    }),
                },
                Err(code) => serde_json::json!({ "id": id, "ok": false, "code": code }),
            };
            push_out(msg.to_string());
        });
}

fn push_out(json: String) {
    if let Ok(mut q) = OUTBOX.lock() {
        if q.len() >= 4096 {
            q.pop_front();
        }
        q.push_back(json);
    }
    let h = HWND_RAW.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_BRIDGE, WPARAM(0), LPARAM(0));
        }
    }
}

/// 在 UI 线程上把队列里的消息推给 WebView（响应 + 事件都走这里）。
pub fn drain() {
    // 先确认 WebView 已经挂上，**再**取队列：否则窗口早期取出来的消息会直接丢掉。
    // 插件加载比 WebView2 就绪早得多，这条路径是常态而不是边缘情况。
    let attached = WEBVIEW.with(|w| w.borrow().is_some());
    if !attached {
        return;
    }
    let items: Vec<String> = {
        let Ok(mut q) = OUTBOX.lock() else { return };
        let n = q.len().min(256);
        q.drain(..n).collect()
    };
    if items.is_empty() {
        return;
    }
    WEBVIEW.with(|w| {
        let borrow = w.borrow();
        let Some(webview) = borrow.as_ref() else {
            return;
        };
        for s in items {
            if let Err(e) = unsafe { webview.PostWebMessageAsJson(&HSTRING::from(s.as_str())) } {
                log::warn(&format!("推送消息失败: {e}"));
            }
        }
    });
}

/// 把总线里攒着的事件推给页面。这条路用于插件的 `emit`。
pub fn flush_events() {
    for (topic, payload) in bus::get().drain_events(64) {
        let value = serde_json::from_slice::<serde_json::Value>(&payload).unwrap_or_else(|_| {
            serde_json::Value::String(String::from_utf8_lossy(&payload).into_owned())
        });
        push_out(
            serde_json::json!({ "type": "event", "topic": topic, "payload": value }).to_string(),
        );
    }
}

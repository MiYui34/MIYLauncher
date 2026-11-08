//! 网络层：只有 WinHTTP，没有自带的 TLS 栈。
//!
//! 这是被 2 MB 体积目标逼出来的选择（架构文档 §12）：rustls + ring 一组
//! 就要 ~1.2 MB，比整个宿主还大。WinHTTP 走系统的 SChannel，代价是
//! "只在 Windows 上跑"——本来就是前提。

use std::ffi::c_void;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpCrackUrl, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
    WinHttpGetIEProxyConfigForCurrentUser, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
    WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_CURRENT_USER_IE_PROXY_CONFIG,
    WINHTTP_FLAG_SECURE, WINHTTP_INTERNET_SCHEME, WINHTTP_OPEN_REQUEST_FLAGS,
    WINHTTP_OPTION_SECURITY_FLAGS,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_RAW_HEADERS_CRLF, WINHTTP_QUERY_STATUS_CODE,
    URL_COMPONENTS,
};

// 别在 UA 里写 `+https://…`：BMCLAPI 的 `/maven/` 会 302 到
// `mirrors.cernet.edu.cn`，那边的 WAF 见到带 `+http` 的 User-Agent 直接回 403
// （实测：`MIYLauncher/0.1 (Windows; +https://…)` → 403，去掉后 → 200）。
// 这类"看起来像脚本"的 UA 特征在镜像站上很常见，报错的却是我们的下载逻辑。
const USER_AGENT: &str = "MIYLauncher/0.1 (Windows; Minecraft launcher)";
const CHUNK: usize = 64 * 1024;

/// 一次 GET 的结果。
pub struct Response {
    pub status: u32,
    pub body: Vec<u8>,
    /// 3xx 时的跳转目标，供上层决定要不要跟。
    pub location: Option<String>,
    /// 429/503 时服务器要求的等待秒数（`Retry-After`）。
    pub retry_after: Option<u64>,
}

/// 下载失败的结构化描述。
///
/// 上层（下载引擎）要据此决定「重试还是放弃」「等多久」「是不是该退回整包」，
/// 所以不能只给一句字符串——字符串匹配既脆又没法区分 404 和 503。
#[derive(Debug, Clone)]
pub struct NetError {
    /// `http` / `norange` / `range` / `io` / `net`
    pub kind: &'static str,
    pub status: u32,
    pub retry_after: Option<u64>,
    pub msg: String,
}

impl NetError {
    fn new(kind: &'static str, msg: impl Into<String>) -> Self {
        Self { kind, status: 0, retry_after: None, msg: msg.into() }
    }
}

/// 句柄的逆序关闭。WinHTTP 的句柄必须按打开的相反顺序关，漏一个就泄漏
/// 一个内核对象——用 Drop 表达最省事。
///
/// 会话是**进程级共享**的：`WinHttpOpen` 带自动代理探测，每请求新开一个会话
/// 就要重做一次 WPAD 解析，实测单请求能从瞬间涨到好几秒；共享之后连接池也
/// 能复用，连续下载快得多。
struct Handles {
    connect: *mut c_void,
    request: *mut c_void,
}

impl Drop for Handles {
    fn drop(&mut self) {
        unsafe {
            if !self.request.is_null() {
                let _ = WinHttpCloseHandle(self.request);
            }
            if !self.connect.is_null() {
                let _ = WinHttpCloseHandle(self.connect);
            }
        }
    }
}

/// 进程级会话，进程退出时才由系统回收。
///
/// 代理按"系统已配置的那个"来，而不是每次自动探测：`AUTOMATIC_PROXY` 会做
/// WPAD 解析，在探测不通的网络里每次会话要等十来秒（实测单请求 0.9 s →
/// 15 s，而 .NET 同一台机器上是 4 s）。
fn session() -> *mut c_void {
    static SESSION: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *SESSION.get_or_init(|| unsafe {
        let agent = wide(USER_AGENT);
        let mut cfg = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
        let ok = WinHttpGetIEProxyConfigForCurrentUser(&mut cfg).is_ok();
        // 系统没配代理、也没开自动探测时走 NO_PROXY：省掉 WPAD 解析。
        // 配了代理（或开了自动探测）就交给 AUTOMATIC_PROXY，别自作主张绕开它——
        // 企业网里绕开代理等于直接断网。
        let has_proxy = ok && !cfg.lpszProxy.is_null() && !pcwstr_to_string(cfg.lpszProxy).is_empty();
        let access = if ok && !cfg.fAutoDetect.as_bool() && !has_proxy {
            WINHTTP_ACCESS_TYPE_NO_PROXY
        } else {
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY
        };
        let sess = WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            access,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        );
        if !sess.is_null() {
            // WinHTTP 默认每个服务器只开 **6** 条连接。下载线程开 12 个也没用，
            // 实际同时在跑的只有 6 条 —— 这是"并发提不上去"的第一原因。
            // 12 = WINHTTP_OPTION_MAX_CONNS_PER_SERVER，13 = ..._PER_1_0_SERVER。
            for opt in [12u32, 13u32] {
                // 12 = WINHTTP_OPTION_MAX_CONNS_PER_SERVER，13 = ..._PER_1_0_SERVER
                let n: u32 = 64;
                let _ = WinHttpSetOption(
                    Some(sess as *const c_void),
                    opt,
                    Some(&n.to_le_bytes()),
                );
            }
        }
        sess as usize
    }) as *mut c_void
}

fn pcwstr_to_string(p: windows::core::PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe {
        let mut n = 0usize;
        while *p.0.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p.0, n))
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 拆 URL。WinHTTP 自己会做，但它要的是 `URL_COMPONENTS` 里的一堆指针
/// 技巧，包一层比到处重复强。
fn split(url: &str) -> Result<(String, String, u16, bool), String> {
    let mut buf = wide(url);
    let mut uc = URL_COMPONENTS {
        dwStructSize: std::mem::size_of::<URL_COMPONENTS>() as u32,
        dwHostNameLength: u32::MAX,
        dwUrlPathLength: u32::MAX,
        dwSchemeLength: u32::MAX,
        dwExtraInfoLength: u32::MAX,
        ..Default::default()
    };
    unsafe { WinHttpCrackUrl(&mut buf, 0, &mut uc) }.map_err(|e| format!("URL 解析失败：{e}"))?;

    let host = unsafe {
        String::from_utf16_lossy(std::slice::from_raw_parts(
            uc.lpszHostName.0,
            uc.dwHostNameLength as usize,
        ))
    };
    let mut path = unsafe {
        String::from_utf16_lossy(std::slice::from_raw_parts(
            uc.lpszUrlPath.0,
            uc.dwUrlPathLength as usize,
        ))
    };
    if uc.dwExtraInfoLength > 0 && !uc.lpszExtraInfo.is_null() {
        path.push_str(&unsafe {
            String::from_utf16_lossy(std::slice::from_raw_parts(
                uc.lpszExtraInfo.0,
                uc.dwExtraInfoLength as usize,
            ))
        });
    }
    if host.is_empty() {
        return Err("URL 里没有主机名".into());
    }
    let https = uc.nScheme == WINHTTP_INTERNET_SCHEME(2); // INTERNET_SCHEME_HTTPS
    let port = if uc.nPort != 0 {
        uc.nPort
    } else if https {
        443
    } else {
        80
    };
    Ok((host, path, port, https))
}

pub fn get_limited(url: &str, timeout: Duration, max_bytes: usize) -> Result<Response, String> {
    send("GET", url, &[], None, timeout, max_bytes)
}

/// `application/x-www-form-urlencoded` 的 POST（OAuth 的设备码与令牌端点都要这个）。
pub fn post_form(
    url: &str,
    form: &str,
    timeout: Duration,
    max_bytes: usize,
) -> Result<Response, String> {
    send(
        "POST",
        url,
        &[("Content-Type", "application/x-www-form-urlencoded")],
        Some(form.as_bytes()),
        timeout,
        max_bytes,
    )
}

/// `application/json` 的 POST（XBL / XSTS / MC 认证都是这个）。
pub fn post_json(
    url: &str,
    json: &str,
    timeout: Duration,
    max_bytes: usize,
) -> Result<Response, String> {
    send(
        "POST",
        url,
        &[
            ("Content-Type", "application/json"),
            ("Accept", "application/json"),
        ],
        Some(json.as_bytes()),
        timeout,
        max_bytes,
    )
}

/// 通用请求：自动跟最多 5 次重定向，然后返回最终响应。
///
/// BMCLAPI 的 `/maven/…` 会 302 到 `mirrors.cernet.edu.cn`（自己的 CDN），
/// 而 WinHTTP 的 C 接口**默认不跟重定向**——不处理就会把 302 当失败，或者
/// 跟着一个我们没设过头的目标吃 403。本来以为镜像不带重定向，实测才发现必须跟。
pub fn send(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    timeout: Duration,
    max_bytes: usize,
) -> Result<Response, String> {
    let mut current = url.to_string();
    for _hop in 0..5 {
        let resp = send_once(method, &current, headers, body, timeout, max_bytes)?;
        let follow = matches!(resp.status, 301 | 302 | 303 | 307 | 308);
        if !follow {
            return Ok(resp);
        }
        let Some(loc) = resp.location.clone() else {
            return Ok(resp);
        };
        current = resolve_url(&current, &loc);
    }
    Err("重定向次数过多".into())
}

/// 把 `Location` 解析成绝对 URL（相对路径按 RFC 3986 的常见情形处理）。
fn resolve_url(base: &str, loc: &str) -> String {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    let Some(pos) = base.find("://") else {
        return loc.to_string();
    };
    let after = pos + 3;
    let rest = &base[after..];
    let host_end = rest.find('/').map(|i| after + i).unwrap_or(base.len());
    let origin = &base[..host_end];
    if loc.starts_with('/') {
        format!("{origin}{loc}")
    } else {
        let dir_end = base.rfind('/').map(|i| i + 1).unwrap_or(base.len());
        format!("{}{loc}", &base[..dir_end])
    }
}

/// 一次请求，不跟重定向。
fn send_once(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    timeout: Duration,
    max_bytes: usize,
) -> Result<Response, String> {
    let (host, path, port, https) = split(url)?;
    let host_w = wide(&host);
    let path_w = wide(&path);
    let method_w = wide(method);
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;

    let mut hs = Handles {
        connect: std::ptr::null_mut(),
        request: std::ptr::null_mut(),
    };

    unsafe {
        let sess = session();
        if sess.is_null() {
            return Err("WinHttpOpen 失败".into());
        }
        // 超时按整次请求给，不是按块给：慢速滴流也要能被打断。
        let _ = WinHttpSetTimeouts(sess, ms, ms, ms, ms);

        hs.connect = WinHttpConnect(sess, PCWSTR(host_w.as_ptr()), port, 0);
        if hs.connect.is_null() {
            return Err(format!("连接 {host} 失败"));
        }

        let flags = if https {
            WINHTTP_OPEN_REQUEST_FLAGS(WINHTTP_FLAG_SECURE.0)
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS(0)
        };
        hs.request = WinHttpOpenRequest(
            hs.connect,
            PCWSTR(method_w.as_ptr()),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            flags,
        );
        if hs.request.is_null() {
            return Err("WinHttpOpenRequest 失败".into());
        }

        // 关掉证书吊销检查：Schannel 默认会去拉 CRL/OCSP，网络到不了那些服务器时
        // 每次握手要么干等十几秒（实测单次 GET 从 0.9 s 变成 15–20 s），要么直接
        // 失败（0x80072F78 / 0x80072EFD）。我们连的都是微软/Modrinth/镜像这类大站，
        // 吊销检查的收益远小于代价。
        // SECURITY_FLAG_IGNORE_REVOCATION = 0x00000080（windows crate 没导出这个常量）。
        let sec: u32 = 0x0000_0080;
        let _ = WinHttpSetOption(
            Some(hs.request as *const c_void),
            WINHTTP_OPTION_SECURITY_FLAGS,
            Some(&sec.to_le_bytes()),
        );

        let header_w = if headers.is_empty() {
            None
        } else {
            let mut joined = String::new();
            for (k, v) in headers {
                joined.push_str(k);
                joined.push_str(": ");
                joined.push_str(v);
                joined.push_str("\r\n");
            }
            // **不能带结尾的 NUL**：WinHTTP 的 dwHeadersLength 是按字符数算的，
            // 多算一个 NUL 会让 WinHttpSendRequest 直接返回 0x80070057（参数错误）。
            let mut w = wide(&joined);
            w.pop();
            Some(w)
        };
        let header_ptr = header_w.as_deref();
        let body_len = body.map(|b| b.len()).unwrap_or(0);
        let body_ptr = if body_len == 0 {
            None
        } else {
            body.map(|b| b.as_ptr() as *const c_void)
        };

        // 注意参数顺序：windows 0.61 把头部长度从签名里去掉了（由切片长度
        // 反推），所以这里比 C 原型少一个 dwHeadersLength。
        WinHttpSendRequest(
            hs.request,
            header_ptr,
            body_ptr,
            body_len as u32,
            body_len as u32,
            0,
        )
        .map_err(|e| format!("发送请求失败：{e}"))?;
        WinHttpReceiveResponse(hs.request, std::ptr::null_mut())
            .map_err(|e| format!("等响应失败：{e}"))?;

        let mut status: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        let _ = WinHttpQueryHeaders(
            hs.request,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut c_void),
            &mut len,
            std::ptr::null_mut(),
        );

        let mut body_out: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; CHUNK];
        let started = std::time::Instant::now();
        loop {
            let mut avail: u32 = 0;
            // 卡住的连接不该占着线程等到总超时：`WinHttpQueryDataAvailable` 会一直
            // 阻塞到有数据或接收超时，所以在这里量"这一次等了多久"。
            let wait_from = std::time::Instant::now();
            WinHttpQueryDataAvailable(hs.request, &mut avail)
                .map_err(|e| format!("读响应失败：{e}"))?;
            if wait_from.elapsed().as_secs() >= 15 {
                return Err(format!(
                    "连接卡住超过 15 秒，已断开重试（{url}，已收到 {} 字节）",
                    body_out.len()
                ));
            }
            if avail == 0 {
                break;
            }
            let mut read: u32 = 0;
            WinHttpReadData(
                hs.request,
                chunk.as_mut_ptr() as *mut c_void,
                avail.min(CHUNK as u32),
                &mut read,
            )
            .map_err(|e| format!("读数据失败：{e}"))?;
            if read == 0 {
                break;
            }
            body_out.extend_from_slice(&chunk[..read as usize]);
            // 慢速熔断：速度低于 1 KB/s 且已经跑了 5 秒以上，就断开换源。
            // 一个 1 KB/s 的连接会把整个块拖到 30 秒超时，而它其实早就该被踢掉。
            let elapsed = started.elapsed();
            if elapsed.as_secs() >= 5 {
                let speed = body_out.len() as f64 / elapsed.as_secs_f64();
                if speed < 1024.0 {
                    return Err(format!(
                        "速度过慢（{speed:.0} B/s），已断开重试（{url}，已收到 {} 字节）",
                        body_out.len()
                    ));
                }
            }
            if body_out.len() > max_bytes {
                // 把地址和实际大小带上：玩家截图报错时能直接看出是哪个请求、差多少。
                return Err(format!(
                    "响应超过 {max_bytes} 字节上限（{url}，已读到 {} 字节）",
                    body_out.len()
                ));
            }
        }

        // 只有可能是重定向、限流或成功时才去抠响应头：正常响应里我们只关心限流额度。
        let raw = if matches!(status, 301 | 302 | 303 | 307 | 308) || status == 429 || status == 503
            || status == 200
            || status == 206
        {
            raw_headers(hs.request)
        } else {
            None
        };
        let location = raw.as_deref().and_then(parse_location);
        let retry_after = raw.as_deref().and_then(parse_retry_after);
        let rl_short = raw.as_deref().and_then(|r| rate_pair(r, "short"));
        let rl_long = raw.as_deref().and_then(|r| rate_pair(r, "long"));
        note_rate(rl_short, rl_long);

        Ok(Response {
            status,
            body: body_out,
            location,
            retry_after,
        })
    }
}

/// 取原始响应头（UTF-16，`\r\n` 分隔）。
fn raw_headers(request: *mut c_void) -> Option<String> {
    let mut buf = vec![0u16; 8192];
    let mut len = (buf.len() * 2) as u32;
    let ok = unsafe {
        WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_RAW_HEADERS_CRLF,
            PCWSTR::null(),
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut len,
            std::ptr::null_mut(),
        )
    };
    if ok.is_err() {
        return None;
    }
    let chars = (len as usize / 2).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..chars]))
}

fn header_value(raw: &str, name: &str) -> Option<String> {
    for line in raw.split("\r\n") {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix(&format!("{name}:")) {
            let start = line.len() - rest.len();
            return Some(line[start..].trim().to_string());
        }
    }
    None
}

fn parse_location(raw: &str) -> Option<String> {
    header_value(raw, "location")
}

/// `Retry-After` 有两种写法：秒数，或 HTTP 日期。日期这种少见，解析失败就当没有。
fn parse_retry_after(raw: &str) -> Option<u64> {
    let v = header_value(raw, "retry-after")?;
    if let Ok(sec) = v.trim().parse::<u64>() {
        return Some(sec.min(300));
    }
    None
}

/// 读一组 `X-RateLimit-Remaining-<win>` / `X-RateLimit-Reset-<win>`。
fn rate_pair(raw: &str, win: &str) -> Option<(u32, u32)> {
    let rem = header_value(raw, &format!("x-ratelimit-remaining-{win}"))?;
    let reset = header_value(raw, &format!("x-ratelimit-reset-{win}"))?;
    let rem: u32 = rem.trim().parse().ok()?;
    let reset: u32 = reset.trim().parse().ok()?;
    Some((rem, reset))
}

/// 最近一次响应里服务器自报的限流额度，供下载引擎排队用。
static RATE: Mutex<(Option<(u32, u32)>, Option<(u32, u32)>)> = Mutex::new((None, None));

fn note_rate(short: Option<(u32, u32)>, long: Option<(u32, u32)>) {
    if short.is_none() && long.is_none() {
        return;
    }
    if let Ok(mut r) = RATE.lock() {
        if short.is_some() {
            r.0 = short;
        }
        if long.is_some() {
            r.1 = long;
        }
    }
}

/// 取走最近一次额度（取走即清空，避免旧数据被反复当成新数据）。
pub fn take_rate() -> (Option<(u32, u32)>, Option<(u32, u32)>) {
    RATE.lock()
        .map(|mut r| (r.0.take(), r.1.take()))
        .unwrap_or((None, None))
}

/// 把 URL 下到文件。先写 `.part` 再改名，中途失败不会留下半个文件骗过
/// 下一次启动。
pub fn download(url: &str, to: &Path, timeout: Duration) -> Result<u64, String> {
    let resp = get_limited(url, timeout, 512 * 1024 * 1024)?;
    if resp.status != 200 {
        return Err(format!("HTTP {}", resp.status));
    }
    // 镜像被限流时有的节点会回 200 + 一小段错误文本，不认这个就会变成
    // 后面莫名其妙的「校验失败」。小于 1 KiB 的响应一律当错误处理。
    if resp.body.len() < 1024 {
        let text = String::from_utf8_lossy(&resp.body);
        let text = text.trim();
        let text = if text.is_empty() { "空响应" } else { text };
        return Err(format!("服务器返回了异常内容：{text}"));
    }
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败：{e}"))?;
    }
    let tmp = to.with_extension("part");
    std::fs::write(&tmp, &resp.body).map_err(|e| format!("写文件失败：{e}"))?;
    // Windows 上 rename 不覆盖已存在的目标，重下同一个文件会卡在这里。
    let _ = std::fs::remove_file(to);
    if let Err(e) = std::fs::rename(&tmp, to) {
        let _ = std::fs::remove_file(to);
        std::fs::rename(&tmp, to).map_err(|e2| format!("改名失败：{e} / {e2}"))?;
    }
    Ok(resp.body.len() as u64)
}

/// 按字节区间取一段，直接写进文件对应偏移处（HTTP `Range`）。
///
/// 单个大文件（客户端 jar 四十多兆）在这类镜像上是"每连接限速"的，
/// 一条连接 200 KB/s，四条就是 800 KB/s —— 分段并行比多开文件更划算。
/// 服务器不认 Range（回 200 整包）时返回错误，调用方回退到整文件下载。
pub fn download_range(
    url: &str,
    to: &Path,
    offset: u64,
    length: u64,
    timeout: Duration,
) -> Result<u64, NetError> {
    if length == 0 {
        return Ok(0);
    }
    let header = format!("bytes={}-{}", offset, offset + length - 1);
    // 上限给「本段长度 + 1 MiB」：服务器不认 Range 时会把整包塞回来，
    // 那种情况下我们只想尽快认出「这不是 206」，而不是把 40 MB 全读进内存。
    let cap = length as usize + (1 << 20);
    let resp = match send(
        "GET",
        url,
        &[("Range", header.as_str())],
        None,
        timeout,
        cap,
    ) {
        Ok(r) => r,
        Err(e) if e.contains("字节上限") => {
            // 读超了上限 = 服务器无视 Range 直接回了整包。
            return Err(NetError {
                kind: "norange",
                status: 200,
                retry_after: None,
                msg: "服务器不支持分段下载".into(),
            });
        }
        Err(e) => return Err(NetError::new("net", e)),
    };
    if resp.status == 200 {
        // 有的 CDN 会直接把整包塞回来：这段不能再往偏移处写，交给调用方处理。
        return Err(NetError {
            kind: "norange",
            status: 200,
            retry_after: None,
            msg: "服务器不支持分段下载".into(),
        });
    }
    if resp.status != 206 {
        return Err(NetError {
            kind: "http",
            status: resp.status,
            retry_after: resp.retry_after,
            msg: format!("HTTP {}", resp.status),
        });
    }
    if resp.body.len() as u64 != length {
        return Err(NetError {
            kind: "range",
            status: 206,
            retry_after: None,
            msg: format!(
                "分段长度不符：期望 {length} 字节，实得 {}",
                resp.body.len()
            ),
        });
    }
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).map_err(|e| NetError::new("io", format!("建目录失败：{e}")))?;
    }
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(to)
        .map_err(|e| NetError::new("io", format!("打开文件失败：{e}")))?;
    f.seek(SeekFrom::Start(offset))
        .map_err(|e| NetError::new("io", format!("定位失败：{e}")))?;
    f.write_all(&resp.body)
        .map_err(|e| NetError::new("io", format!("写文件失败：{e}")))?;
    f.flush()
        .map_err(|e| NetError::new("io", format!("刷新失败：{e}")))?;
    Ok(resp.body.len() as u64)
}

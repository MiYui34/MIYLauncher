//! 正版登录（Microsoft → Xbox Live → XSTS → Minecraft）。
//!
//! 只有一条路：**设备码流程**。启动器显示 `AB12-CD34`，用户去
//! `microsoft.com/link` 输入并同意，启动器按 interval 轮询换令牌。
//! 不需要客户端密钥，也不需要重定向 URI，启动器全程碰不到用户密码。
//!
//! 令牌只在这里存在，**从不经过** WebView2：页面能调到的是
//! `miy.accounts.start/poll/list/logout`，拿不到 `access_token`。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::log;
use crate::net;
use crate::paths;

const SCOPE: &str = "XboxLive.signin offline_access";

/// 授权端点的租户段。
///
/// 应用注册时选「仅个人 Microsoft 帐户」→ 用 `consumers`；
/// 选「任何组织目录 + 个人账户」（应用挂在某个组织目录下）→ 必须用 `common`，
/// 否则微软回 `AADSTS700016`（在租户里找不到这个应用）。两种都能登录个人账户，
/// 区别只是应用被登记在哪个目录，所以交给设置项，默认 `consumers`。
/// 私有凭据文件：`<data>/config/auth.json`，形如
/// `{"client_id":"…","authority":"consumers"}`。
///
/// 客户端 ID 故意不放进设置系统——设置页会把每一项都渲染出来，而这个 ID
/// 属于"知道的人越少越好"的那一类（截图、直播、求助时都不该带出去）。
fn auth_json() -> Option<Value> {
    let text = std::fs::read_to_string(paths::auth_file()).ok()?;
    serde_json::from_str(&text).ok()
}

fn auth_field(key: &str) -> String {
    auth_json()
        .and_then(|v| v.get(key).and_then(|x| x.as_str()).map(str::to_string))
        .unwrap_or_default()
        .trim()
        .to_string()
}


/// 日志与报错里打印客户端 ID 一律走这个。
fn mask(id: &str) -> String {
    let n = id.chars().count();
    if n <= 8 {
        return "****".to_string();
    }
    let head: String = id.chars().take(4).collect();
    let tail: String = id.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

/// 登录端点。
///
/// 只在 `auth.json` 里配（`{"authority":"common"}`），默认 `consumers`。
/// 它不做成设置项：普通用户永远不需要改，而把它摆在设置页只会让人以为
/// 登录坏了是这里的问题。
fn authority() -> String {
    let from_file = auth_field("authority");
    if from_file.is_empty() {
        "consumers".to_string()
    } else {
        from_file
    }
}

fn oauth_url(kind: &str) -> String {
    format!("https://login.microsoftonline.com/{}/oauth2/v2.0/{kind}", authority())
}

const XBL_URL: &str = "https://user.auth.xboxlive.com/user/authenticate";
const XSTS_URL: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";
const MC_LOGIN_URL: &str = "https://api.minecraftservices.com/authentication/login_with_xbox";
const MC_PROFILE_URL: &str = "https://api.minecraftservices.com/minecraft/profile";

/// 当前登录的账号。令牌过期前 60 s 就会先换新的，所以这里的
/// `access_token` 随时可被 `token()` 取走用。
struct Session {
    name: String,
    uuid: String,
    /// Minecraft 的 access_token（不是微软的）。
    access_token: String,
    /// 微软的 refresh_token，落盘时是 DPAPI 密文。
    refresh_token: String,
    /// Minecraft 令牌有效期（一般 24 h）。
    expires_at: Instant,
}

/// 一次正在进行中的登录：设备码，用户把码输进去，我们按 interval 轮询。
struct Pending {
    device_code: String,
    client_id: String,
    interval: u64,
    expires_at: Instant,
    next_poll: Instant,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

/// 编进程序的 Microsoft 客户端 ID。
///
/// 以前要用户自己在 `config/auth.json` 里填一个 GUID 才能登录 —— 这一步没人
/// 猜得出来，取消掉。仍然允许被覆盖（`MIY_LAUNCHER_CLIENT_ID` 环境变量或
/// `auth.json`），方便开发者换自己的应用注册。
pub const DEFAULT_CLIENT_ID: &str = "e6c5b06b-c412-4579-ab72-8e2679d29956";

fn client_id() -> String {
    // 优先顺序：环境变量（CI/临时调试）→ 私有凭据文件 → 编进程序的那个。
    let from_env = std::env::var("MIY_LAUNCHER_CLIENT_ID")
        .unwrap_or_default()
        .trim()
        .to_string();
    if !from_env.is_empty() {
        return from_env;
    }
    let from_file = auth_field("client_id");
    if !from_file.is_empty() {
        return from_file;
    }
    DEFAULT_CLIENT_ID.to_string()
}

/// 给探针/日志用：只暴露"配没配"和打码后的样子。
pub struct ClientIdHint {
    pub has: bool,
    pub masked: String,
}

pub fn client_id_hint() -> ClientIdHint {
    let cid = client_id();
    ClientIdHint {
        has: !cid.is_empty(),
        masked: mask(&cid),
    }
}

fn account_file() -> PathBuf {
    paths::data_root().join("accounts").join("current.bin")
}

/// 启动时把上次的账号读回来。读不出来就当没登录——**绝不让登录状态挡住
/// 启动器**，大不了让用户再登一次。
pub fn init() {
    let path = account_file();
    let Ok(blob) = std::fs::read(&path) else {
        return;
    };
    let Ok(plain) = dpapi::unprotect(&blob) else {
        log::warn("账号文件解不开（换了机器或用户？），当成未登录");
        return;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&plain) else {
        log::warn("账号文件不是合法 JSON，忽略");
        return;
    };
    let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let uuid = v.get("uuid").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let rt = v.get("rt").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if name.is_empty() || uuid.is_empty() || rt.is_empty() {
        return;
    }
    log::info(&format!("已载入账号 {name}（令牌待刷新）"));
    // 记住身份，但令牌要等到真正要用时再换——启动阶段不该发起网络请求。
    if let Ok(mut g) = SESSION.lock() {
        *g = Some(Session {
            name,
            uuid,
            access_token: String::new(),
            refresh_token: rt,
            expires_at: Instant::now(),
        });
    }
    // 账号列表里记着「上次用的是谁」，以它为准（离线账号也走这条路）。
    let (items, active) = load_store();
    if !active.is_empty() {
        apply_active(&items, &active);
    }
    // 老版本只存「当前这一个账号」，升级后补进列表，否则账号菜单是空的。
    if items.is_empty() {
        let cur = SESSION.lock().ok().and_then(|g| {
            g.as_ref()
                .map(|s| (s.name.clone(), s.uuid.clone(), s.refresh_token.clone()))
        });
        match cur {
            Some((name, uuid, rt)) => {
                let mut list = Vec::new();
                upsert(
                    &mut list,
                    Profile {
                        name,
                        uuid: uuid.clone(),
                        kind: "msa".into(),
                        rt,
                    },
                );
                if let Err(e) = save_store(&list, &uuid) {
                    log::warn(&format!("账号列表写入失败：{e}"));
                }
            }
            None => {
                let o = offline_get();
                let name = o["name"].as_str().unwrap_or("").to_string();
                let uuid = o["uuid"].as_str().unwrap_or("").to_string();
                if !name.is_empty() && !uuid.is_empty() {
                    remember_offline(&name, &uuid);
                }
            }
        }
    }
}

fn save(s: &Session) -> Result<(), String> {
    let payload = json!({ "name": s.name, "uuid": s.uuid, "rt": s.refresh_token }).to_string();
    let blob = dpapi::protect(payload.as_bytes())?;
    let path = account_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败：{e}"))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &blob).map_err(|e| format!("写账号文件失败：{e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("改名失败：{e}"))?;
    Ok(())
}

fn post_json(url: &str, body: Value, timeout: Duration) -> Result<Value, String> {
    let r = net::post_json(url, &body.to_string(), timeout, 1024 * 1024)?;
    let text = String::from_utf8_lossy(&r.body).to_string();
    if r.status != 200 {
        // XBL/XSTS 的错误体是 {"Identity":"0","XErr":2148916233,...}，把 XErr 单独拎出来
        // 才有机会翻成人话（见 explain_xsts）。
        let v = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
        let xerr = v
            .get("XErr")
            .and_then(|x| x.as_u64())
            .map(|n| n.to_string())
            .unwrap_or_default();
        if !xerr.is_empty() {
            return Err(explain_xsts(xerr));
        }
        return Err(format!(
            "HTTP {} {}",
            r.status,
            first_line(text.trim())
        ));
    }
    serde_json::from_str::<Value>(&text).map_err(|e| format!("响应不是 JSON：{e}"))
}

/// 拿原始状态码 + JSON。设备码轮询的 400 里带着 `authorization_pending`，
/// 那是**正常状态**，不能在这里被当成失败吞掉。
fn post_form_raw(url: &str, form: &str, timeout: Duration) -> Result<(u32, Value), String> {
    let r = net::post_form(url, form, timeout, 1024 * 1024)?;
    let text = String::from_utf8_lossy(&r.body).to_string();
    let v = serde_json::from_str::<Value>(&text).unwrap_or_else(|_| json!({ "raw": text }));
    Ok((r.status, v))
}

fn post_form(url: &str, form: &str, timeout: Duration) -> Result<Value, String> {
    let (status, v) = post_form_raw(url, form, timeout)?;
    if status == 200 {
        Ok(v)
    } else {
        // 微软的错误体里 error_description 才是人话（AADSTS700038 …），
        // error 只是个短码，拿去给用户看等于没说。
        let code = v.get("error").and_then(|x| x.as_str()).unwrap_or("请求失败");
        let msg = v
            .get("error_description")
            .and_then(|x| x.as_str())
            .map(|s| crate::accounts::first_line(s))
            .unwrap_or_default();
        if msg.is_empty() {
            Err(format!("HTTP {status} {code}"))
        } else {
            Err(format!("HTTP {status} {code}：{msg}"))
        }
    }
}

/// 微软的 `error_description` 常常很长还带 Trace ID，只留第一句。
pub(crate) fn first_line(s: &str) -> String {
    let head = s.split("Trace ID").next().unwrap_or(s).trim();
    head.chars().take(200).collect()
}

/* ---------------- 设备码（回退路径） ---------------- */

/// 设备码：拿到"请用户去输入的那串码"。
fn start_device(cid: &str) -> Result<Value, String> {
    let cid = cid.to_string();
    let form = format!("client_id={}&scope={}", enc(&cid), enc(SCOPE));
    let v = post_form(&oauth_url("devicecode"), &form, Duration::from_secs(30))?;

    let device_code = v.get("device_code").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let user_code = v.get("user_code").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let verify = v
        .get("verification_uri")
        .and_then(|x| x.as_str())
        .unwrap_or("https://microsoft.com/link")
        .to_string();
    let expires_in = v.get("expires_in").and_then(|x| x.as_u64()).unwrap_or(900);
    let interval = v.get("interval").and_then(|x| x.as_u64()).unwrap_or(5).max(1);
    if device_code.is_empty() || user_code.is_empty() {
        return Err("设备码响应缺字段".into());
    }

    if let Ok(mut g) = PENDING.lock() {
        *g = Some(Pending {
            device_code,
            client_id: cid,
            interval,
            expires_at: Instant::now() + Duration::from_secs(expires_in),
            next_poll: Instant::now(),
        });
    }
    log::info(&format!("设备码已取到：{user_code}（{expires_in}s 内有效）"));
    Ok(json!({
        "ok": true,
        "mode": "device",
        "user_code": user_code,
        "verify_uri": verify,
        "expires_in": expires_in,
        "interval": interval,
        "wait_ms": interval * 1000,
    }))
}

/// 设备码：轮询。用户还没点头时返回 `pending`，别把它当错误。
fn poll_device() -> Result<Value, String> {
    let (device_code, cid, expires_at, next_poll) = {
        let Ok(g) = PENDING.lock() else {
            return Err("内部锁坏了".into());
        };
        let Some(p) = g.as_ref() else {
            return Ok(json!({ "status": "idle" }));
        };
        (p.device_code.clone(), p.client_id.clone(), p.expires_at, p.next_poll)
    };
    let now = Instant::now();
    if now > expires_at {
        clear_pending();
        return Ok(json!({ "status": "expired" }));
    }
    if now < next_poll {
        // 轮询间隔是微软规定的，抢跑只会拿回 slow_down。
        return Ok(json!({ "status": "pending", "wait_ms": (next_poll - now).as_millis() as u64 }));
    }
    bump_next_poll();

    let form = format!(
        "grant_type=urn:ietf:params:oauth:grant-type:device_code&device_code={}&client_id={}",
        enc(&device_code),
        enc(&cid)
    );
    let (status, v) = post_form_raw(&oauth_url("token"), &form, Duration::from_secs(30))?;
    // 每一次轮询都留痕（debug 级）。用户说"授权了但启动器没反应"时，
    // 这一行是唯一能分清"没在轮询"和"轮询了但微软说还没授权"的证据。
    crate::log::debug(&format!(
        "设备码轮询：HTTP {status} {}",
        v.get("error").and_then(|x| x.as_str()).unwrap_or("ok")
    ));
    if status != 200 {
        let err = v.get("error").and_then(|x| x.as_str()).unwrap_or("");
        return match err {
            // 用户还没在浏览器里点完，属于正常状态。
            "authorization_pending" => Ok(json!({ "status": "pending" })),
            "slow_down" => Ok(json!({ "status": "pending", "wait_ms": 6000 })),
            "expired_token" => {
                clear_pending();
                Ok(json!({ "status": "expired" }))
            }
            "authorization_declined" => {
                clear_pending();
                Ok(json!({ "status": "declined" }))
            }
            _ => {
                clear_pending();
                let desc = v
                    .get("error_description")
                    .and_then(|x| x.as_str())
                    .unwrap_or("未知错误");
                Err(format!("登录失败：HTTP {status} {desc}"))
            }
        };
    }

    let ms_access = v.get("access_token").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let refresh = v.get("refresh_token").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if ms_access.is_empty() || refresh.is_empty() {
        return Err("令牌响应缺字段".into());
    }
    let result = match finish_login(&ms_access, &refresh) {
        Ok(v) => v,
        Err(e) => {
            // 走到这一步说明微软那边已经授权成功，是后面 XBL/XSTS/MC 出的问题。
            crate::log::warn(&format!("拿到微软令牌了，但换 MC 身份失败：{e}"));
            clear_pending();
            return Err(e);
        }
    };
    crate::log::info("登录成功：微软令牌 → XBL → XSTS → MC 档案 全通");
    clear_pending();
    Ok(result)
}

fn bump_next_poll() {
    if let Ok(mut g) = PENDING.lock() {
        if let Some(p) = g.as_mut() {
            p.next_poll = Instant::now() + Duration::from_secs(p.interval);
        }
    }
}

fn clear_pending() {
    if let Ok(mut g) = PENDING.lock() {
        *g = None;
    }
}

/* ---------------- 配置自检（不打开浏览器） ---------------- */

/// 客户端 ID 自检：不弹浏览器、不发真登录，只把应用注册的配置验一遍。
///
/// 手法是拿一个**故意伪造的授权码**去打 token 端点，再看微软抱怨什么：
///
/// | 回什么 | 说明 |
/// |---|---|
/// | `invalid_grant` / `AADSTS70008` / `AADSTS9002313` | 应用认得，只是码是假的 → 配置没问题 |
/// | `AADSTS700016` / `AADSTS70001` / `AADSTS90002` | 这个端点的目录里没有这个应用 → 登录端点选错 |
/// | `AADSTS50011` / `AADSTS70004` | 重定向 URI 没配 |
/// | `AADSTS700025` / `AADSTS70002` | 被当机密客户端 → 公共客户端流没开 |
///
/// 价值在于：所有失败都能在**不打开浏览器**的情况下定位，省掉
/// "点了登录 → 浏览器报错 → 不知道哪一步错"的来回。
pub fn check() -> Value {
    let v = check_impl();
    log::info(&format!(
        "账号自检：{}（{}）",
        v.get("verdict").and_then(|x| x.as_str()).unwrap_or("?"),
        v.get("detail").and_then(|x| x.as_str()).unwrap_or("")
    ));
    v
}

fn check_impl() -> Value {
    let cid = client_id();
    if cid.is_empty() {
        return json!({
            "ok": false, "verdict": "no-client-id",
            "message": "还没配置客户端 ID：把 auth.json（{\"client_id\":\"…\"}）放进用户数据的 config 目录"
        });
    }

    // 登录只剩设备码一条路，所以自检就是**真的去要一个设备码**：
    // 它同时验证了三件事——应用存在、公共客户端流是开着的、端点选对了。
    // 拿到的码不用（用户没看见也不需要），只用来判配置。
    let form = format!("client_id={}&scope={}", enc(&cid), enc(SCOPE));
    let (status, v) = match post_form_raw(&oauth_url("devicecode"), &form, Duration::from_secs(20)) {
        Ok(x) => x,
        Err(e) => return json!({"ok": false, "verdict": "network", "message": e}),
    };
    let code = v.get("error").and_then(|x| x.as_str()).unwrap_or("");
    let desc = v
        .get("error_description")
        .and_then(|x| x.as_str())
        .map(first_line)
        .unwrap_or_default();
    let blob = format!("{code} {desc}");
    // 微软的报错里会把客户端 ID 原样回显，这里给界面看，一律打码。
    let detail = format!("HTTP {status} {code}：{desc}").replace(&cid, &mask(&cid));

    let has_code = v.get("user_code").and_then(|x| x.as_str()).map(|s| !s.is_empty()).unwrap_or(false);
    if status == 200 && has_code {
        return json!({
            "ok": true, "verdict": "ready", "authority": authority(),
            "message": "登录服务可用。",
            "detail": detail
        });
    }

    let (verdict, message) = if blob.contains("AADSTS700038")
        || blob.contains("not a valid application identifier")
    {
        // 必须排在 `unauthorized_client` 前面：ID 本身不合法时微软也报
        // `unauthorized_client`，先判会让用户白跑一趟「换端点」。
        (
            "bad-client-id",
            "登录配置有误，请联系启动器维护者更新配置。"
                .to_string(),
        )
    } else if blob.contains("AADSTS700016")
        || blob.contains("AADSTS70001")
        || blob.contains("AADSTS90002")
        || blob.contains("unauthorized_client")
    {
        let other = if authority() == "consumers" { "common" } else { "consumers" };
        (
            "wrong-authority",
            format!(
                "这个端点的目录里没有这个应用。多半是账号类型和「登录端点」不匹配：\
                 把它改成 {other} 再自检一次。"
            ),
        )
    } else if blob.contains("AADSTS50011")
        || blob.contains("AADSTS70004")
        || blob.contains("redirect_uri")
    {
        (
            "redirect-uri",
            "重定向 URI 对不上。去「认证」页加平台「移动和桌面应用程序」，\
             自定义 URI 填 http://localhost/callback（端口不用写、路径必须一致）。"
                .to_string(),
        )
    } else if blob.contains("AADSTS700025")
        || blob.contains("AADSTS70002")
        || blob.contains("invalid_client")
    {
        (
            "not-public-client",
            "应用被当成机密客户端了。去「认证」页最下面的「高级设置」，\
             把「允许公共客户端流」改成「是」并保存。"
                .to_string(),
        )
    } else {
        (
            "unknown",
            "登录服务响应异常，请稍后重试。".to_string(),
        )
    };
    json!({
        "ok": false, "verdict": verdict, "authority": authority(),
        "message": message, "detail": detail
    })
}

/* ---------------- 设备码登录（唯一的登录方式） ---------------- */

/// 起一次设备码登录。返回要展示给用户的 user_code 与验证网址。
pub fn start() -> Result<Value, String> {
    let cid = client_id();
    if cid.is_empty() {
        return Err("尚未配置 Microsoft 登录信息。你可以先使用离线身份启动游戏，单机与局域网功能不受影响。".into());
    }
    clear_pending();
    start_device(&cid)
}

/// 第二步的入口：按当前待处理登录的状态返回。
pub fn poll() -> Result<Value, String> {
    let pending = {
        let Ok(g) = PENDING.lock() else {
            return Err("内部锁坏了".into());
        };
        g.is_some()
    };
    if !pending {
        return Ok(json!({ "status": "idle" }));
    }
    poll_device()
}

/// 用户点了取消：把这个待处理登录丢掉就行（没有后台线程要停）。
pub fn cancel() {
    clear_pending();
}

/* ---------------- 微软令牌 → 游戏身份 ---------------- */

/// 微软 access_token 换成 Xbox Live → XSTS → Minecraft 三段令牌。
/// 任一段失败都带上人话解释，别只丢一个错误码给用户。
fn finish_login(ms_access: &str, refresh: &str) -> Result<Value, String> {
    // 1. Xbox Live
    let xbl = post_json(
        XBL_URL,
        json!({
            "Properties": {
                "AuthMethod": "RPS",
                "SiteName": "user.auth.xboxlive.com",
                "RpsTicket": format!("d={ms_access}")
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT"
        }),
        Duration::from_secs(30),
    )
    .map_err(|e| {
        crate::log::warn(&format!("Xbox 账号验证失败：{e}"));
        "Xbox 账号验证未通过，请稍后重试。".to_string()
    })?;
    let xbl_token = xbl.get("Token").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let uhs = xbl
        .get("DisplayClaims")
        .and_then(|d| d.get("xui"))
        .and_then(|x| x.get(0))
        .and_then(|x| x.get("uhs"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if xbl_token.is_empty() || uhs.is_empty() {
        return Err("Xbox 账号验证未完成，请稍后重试。".into());
    }

    // 2. XSTS
    let xsts = post_json(
        XSTS_URL,
        json!({
            "Properties": { "SandboxId": "RETAIL", "UserTokens": [xbl_token] },
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT"
        }),
        Duration::from_secs(30),
    )
    .map_err(explain_xsts)?;
    let xsts_token = xsts.get("Token").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if xsts_token.is_empty() {
        return Err("登录授权未完成，请稍后重试。".into());
    }

    // 3. Minecraft
    let mc = post_json(
        MC_LOGIN_URL,
        json!({ "identityToken": format!("XBL3.0 x={uhs};{xsts_token}") }),
        Duration::from_secs(30),
    )
    .map_err(|e| {
        crate::log::warn(&format!("游戏账号验证失败：{e}"));
        "游戏账号验证未通过，请稍后重试。".to_string()
    })?;
    let mc_token = mc.get("access_token").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let expires_in = mc.get("expires_in").and_then(|x| x.as_u64()).unwrap_or(86_400);
    if mc_token.is_empty() {
        return Err("游戏账号验证未完成，请稍后重试。".into());
    }

    // 4. 档案
    // 这个端点必须带 Bearer —— 少了它拿到的是 401 的
    // `{"path":"/minecraft/profile","errorType":"Unauthorized"}`，
    // 表面上"有 JSON"、但里面没有 name/id，很容易误判成"档案缺字段"。
    let authz = format!("Bearer {mc_token}");
    let prof = net::send(
        "GET",
        MC_PROFILE_URL,
        &[("Authorization", &authz)],
        None,
        Duration::from_secs(30),
        256 * 1024,
    )
    .map_err(|e| {
        crate::log::warn(&format!("读取游戏档案失败：{e}"));
        "无法读取游戏档案，请稍后重试。".to_string()
    })?;
    let text = String::from_utf8_lossy(&prof.body).to_string();
    if prof.status == 404 {
        return Err("该账号尚未拥有 Minecraft: Java 版，或还没有创建游戏角色。".into());
    }
    if prof.status == 401 {
        crate::log::warn(&format!(
            "读取游戏档案被拒（HTTP 401）：{}",
            crate::accounts::first_line(&text)
        ));
        return Err("无法读取游戏档案，请确认该账号已拥有 Minecraft: Java 版。".into());
    }
    if prof.status != 200 {
        crate::log::warn(&format!(
            "读取游戏档案失败（HTTP {}）：{}",
            prof.status,
            first_line(&text)
        ));
        return Err(format!("读取游戏档案失败（HTTP {}），请稍后重试。", prof.status));
    }
    let pv: Value = serde_json::from_str(&text).map_err(|e| {
        crate::log::warn(&format!(
            "游戏档案不是 JSON（HTTP {}）：{e}；响应开头：{}",
            prof.status,
            first_line(&text)
        ));
        "读取游戏档案失败，请稍后重试。".to_string()
    })?;
    let name = pv.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let uuid = pv.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if name.is_empty() || uuid.is_empty() {
        crate::log::warn(&format!("游戏档案缺字段：{}", first_line(&text)));
        return Err("游戏档案信息不完整，请稍后重试。".into());
    }

    let session = Session {
        name: name.clone(),
        uuid: uuid.clone(),
        access_token: mc_token,
        refresh_token: refresh.to_string(),
        expires_at: Instant::now() + Duration::from_secs(expires_in.saturating_sub(60)),
    };
    save(&session)?;
    remember_msa(&session);
    if let Ok(mut g) = SESSION.lock() {
        *g = Some(session);
    }
    log::info(&format!("登录成功：{name}（{uuid}）"));
    Ok(json!({ "status": "ok", "name": name, "uuid": uuid }))
}

/// XSTS 的 401 里带 `XErr`，每个值都对应一句能直接给用户看的话。
fn explain_xsts(err: String) -> String {
    let code = err
        .split("XErr\"")
        .nth(1)
        .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
        .map(|s| s.to_string())
        .unwrap_or_default();
    match code.as_str() {
        "2148916233" => "该账号尚未创建 Xbox 档案，请访问 xbox.com 创建后再试。".to_string(),
        "2148916235" => "该账号所在地区暂不支持 Xbox Live 服务。".to_string(),
        "2148916236" | "2148916237" => "需要先完成该账号的成人验证。".to_string(),
        "2148916238" => "这是未成年人账号，需要家长将账号加入家庭组。".to_string(),
        _ => {
            crate::log::warn(&format!("登录授权失败：{err}"));
            "登录授权失败，请稍后重试。".to_string()
        }
    }
}

/* ---------------- 对外 ---------------- */

/// 给启动流程用的令牌。快过期就先刷新，刷新失败也只是报错，不影响离线启动。
pub fn token() -> Result<Value, String> {
    ensure_fresh()?;
    let g = SESSION.lock().map_err(|_| "内部锁坏了".to_string())?;
    let Some(s) = g.as_ref() else {
        return Err("未登录".into());
    };
    Ok(json!({
        "ok": true,
        "access_token": s.access_token,
        "uuid": s.uuid,
        "name": s.name,
        "user_type": "msa",
    }))
}

fn ensure_fresh() -> Result<(), String> {
    let (need, refresh) = {
        let g = SESSION.lock().map_err(|_| "内部锁坏了".to_string())?;
        match g.as_ref() {
            None => return Err("未登录".into()),
            Some(s) => (
                s.access_token.is_empty() || Instant::now() >= s.expires_at,
                s.refresh_token.clone(),
            ),
        }
    };
    if !need {
        return Ok(());
    }
    let cid = client_id();
    if cid.is_empty() {
        return Err("缺少 Microsoft 客户端 ID，无法刷新令牌".into());
    }
    let form = format!(
        "grant_type=refresh_token&client_id={}&refresh_token={}&scope={}",
        enc(&cid),
        enc(&refresh),
        enc(SCOPE)
    );
    let v = post_form(&oauth_url("token"), &form, Duration::from_secs(30))?;
    let ms_access = v.get("access_token").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if ms_access.is_empty() {
        return Err("登录已过期，请重新登录".into());
    }
    let rt = v
        .get("refresh_token")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .unwrap_or(refresh);
    finish_login(&ms_access, &rt)?;
    Ok(())
}

/// 已登录账号的公开信息——**没有令牌字段**，页面可以放心拿。
pub fn list() -> Value {
    let (items, active) = load_store();
    let cur = identity();
    let cur_name = cur.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let cur_uuid = cur.get("uuid").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let accounts: Vec<Value> = items
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "uuid": p.uuid,
                "type": p.kind,
                "active": p.uuid == active,
            })
        })
        .collect();
    json!({
        "ok": true,
        "accounts": accounts,
        "current": { "name": cur_name, "uuid": cur_uuid },
    })
}

pub fn logout() {
    // 退出登录 = 把这个微软账号从列表里删掉；离线身份不受影响。
    let (mut items, active) = load_store();
    let before = items.len();
    items.retain(|p| p.uuid != active || p.kind != "msa");
    if items.len() != before {
        let next = items.first().map(|p| p.uuid.clone()).unwrap_or_default();
        if let Err(e) = save_store(&items, &next) {
            log::warn(&format!("账号列表写入失败：{e}"));
        }
    }
    if let Ok(mut g) = SESSION.lock() {
        *g = None;
    }
    let path = account_file();
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    log::info("已退出登录");
}

pub fn is_logged_in() -> bool {
    SESSION.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// 表单值编码。账号名里出现 `+`/`&` 就会把请求搅烂，这一步不能省。
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
/* ---------------- DPAPI ---------------- */

mod dpapi {
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    /// 附加熵：同一台机器上的别的程序光复制文件也解不开。
    const ENTROPY: &[u8] = b"MIYLauncher/account/v1";

    fn blob(data: &[u8]) -> (CRYPT_INTEGER_BLOB, Vec<u8>) {
        let mut copy = data.to_vec();
        let b = CRYPT_INTEGER_BLOB {
            cbData: copy.len() as u32,
            pbData: copy.as_mut_ptr(),
        };
        (b, copy)
    }

    fn empty_blob() -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        }
    }

    fn take(out: &CRYPT_INTEGER_BLOB) -> Vec<u8> {
        unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() }
    }

    pub fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
        unsafe {
            let (mut inp, _keep1) = blob(plain);
            let (mut ent, _keep2) = blob(ENTROPY);
            let mut out = empty_blob();
            let ok = CryptProtectData(
                &mut inp,
                std::ptr::null(),
                &mut ent,
                std::ptr::null() as *const c_void,
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            );
            if ok == 0 {
                return Err("DPAPI 加密失败".into());
            }
            let bytes = take(&out);
            LocalFree(out.pbData as *mut c_void);
            Ok(bytes)
        }
    }

    pub fn unprotect(cipher: &[u8]) -> Result<Vec<u8>, String> {
        unsafe {
            let (mut inp, _keep1) = blob(cipher);
            let (mut ent, _keep2) = blob(ENTROPY);
            let mut out = empty_blob();
            let ok = CryptUnprotectData(
                &mut inp,
                std::ptr::null_mut(),
                &mut ent,
                std::ptr::null() as *const c_void,
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            );
            if ok == 0 {
                return Err("DPAPI 解密失败".into());
            }
            let bytes = take(&out);
            LocalFree(out.pbData as *mut c_void);
            Ok(bytes)
        }
    }
}

// ---------------- 离线身份 ----------------
//
// 用户要能自己决定离线账号叫什么、UUID 是什么（PCL2 也是这么做的）。
// 存在 `<data>/config/offline.json`，和正版令牌完全分开：没登录也能改。

fn offline_file() -> PathBuf {
    paths::offline_file()
}

/// 名字规则按原版来：3–16 个字符，只允许字母数字和下划线。
fn valid_name(name: &str) -> bool {
    let n = name.chars().count();
    (3..=16).contains(&n)
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 由名字派生一个稳定的 UUID。
///
/// 和原版离线服一致的算法：`md5("OfflinePlayer:" + name)`，版本位置 3。
///
/// 用别的哈希也能造出"看起来合法"的 UUID，但服务器按名字重算出来的就不是这个，
/// 联机时会被当成另一个人（存档、皮肤、领地全都对不上）。
fn uuid_for(name: &str) -> String {
    // 和原版离线服（以及 PCL2 等启动器）保持一致的算法：
    //   UUID = md5("OfflinePlayer:" + 名字)，再把版本位改成 3。
    // 用别的哈希也能生成"看起来合法"的 UUID，但那样服务器按名字重算出来的
    // 和启动器给的就不是同一个，联机时会被当成不同的人。
    let mut data = b"OfflinePlayer:".to_vec();
    data.extend_from_slice(name.as_bytes());
    let digest = md5(&data);
    let mut s: Vec<char> = digest.iter().map(|b| format!("{b:02x}")).collect::<String>().chars().collect();
    // 版本位（第 13 个十六进制位）固定 3；变体位按 RFC 4122 打 8/9/a/b。
    s[12] = '3';
    s[16] = match s[16] {
        '8' | '9' | 'a' | 'b' => s[16],
        _ => '8',
    };
    let s: String = s.into_iter().collect();
    format!("{}-{}-{}-{}-{}", &s[0..8], &s[8..12], &s[12..16], &s[16..20], &s[20..32])
}

/// MD5（RFC 1321）。没有第三方的加密库，这里只为了算离线 UUID。
fn md5(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let mut k = [0u32; 64];
    for (i, slot) in k.iter_mut().enumerate() {
        *slot = ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32;
    }
    let mut msg = input.to_vec();
    let bits = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0) =
        (0x6745_2301u32, 0xefcd_ab89u32, 0x98ba_dcfeu32, 0x1032_5476u32);
    for chunk in msg.chunks(64) {
        let mut m = [0u32; 16];
        for (i, slot) in m.iter_mut().enumerate() {
            let p = i * 4;
            *slot = u32::from_le_bytes([chunk[p], chunk[p + 1], chunk[p + 2], chunk[p + 3]]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let tmp = d;
            d = c;
            c = b;
            let sum = a
                .wrapping_add(f)
                .wrapping_add(k[i])
                .wrapping_add(m[g]);
            b = b.wrapping_add(sum.rotate_left(S[i]));
            a = tmp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

pub fn offline_get() -> Value {
    let p = offline_file();
    let mut name = String::new();
    let mut uuid = String::new();
    if let Ok(text) = std::fs::read_to_string(&p) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
            uuid = v.get("uuid").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
        }
    }
    if !valid_name(&name) {
        name = "Player".to_string();
    }
    if uuid.is_empty() {
        uuid = uuid_for(&name);
    }
    json!({ "ok": true, "name": name, "uuid": uuid })
}

pub fn offline_set(req: &[u8]) -> Value {
    let v: Value = match serde_json::from_slice(req) {
        Ok(v) => v,
        Err(_) => return json!({ "ok": false, "error": "请求格式不对" }),
    };
    let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    if !valid_name(&name) {
        return json!({
            "ok": false,
            "error": "玩家名需要 3–16 个字符，只能使用字母、数字和下划线"
        });
    }
    let uuid = v.get("uuid").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let uuid = if uuid.is_empty() { uuid_for(&name) } else { uuid };
    if uuid.len() != 36 || uuid.chars().filter(|c| *c == '-').count() != 4 {
        return json!({ "ok": false, "error": "身份标识格式不正确" });
    }
    let doc = json!({ "name": name, "uuid": uuid });
    let p = offline_file();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&p, doc.to_string()) {
        return json!({ "ok": false, "error": format!("写不进 {}：{e}", p.display()) });
    }
    remember_offline(&name, &uuid);
    log::info(&format!("离线身份改成 {name}（{uuid}）"));
    json!({ "ok": true, "name": name, "uuid": uuid })
}

// ---- 多账号 ---------------------------------------------------------------
//
// 账号列表单独存一份（DPAPI 加密），`current.bin` 只记「当前是谁」。
// 这样切换账号不用重新登录，也不会因为一次登录失败把原来的账号弄丢。

fn profiles_file() -> PathBuf {
    paths::data_root().join("accounts").join("profiles.json")
}

#[derive(Clone)]
struct Profile {
    name: String,
    uuid: String,
    /// "msa" 或 "offline"
    kind: String,
    /// 微软账号的 refresh_token；离线账号为空。
    rt: String,
}

fn load_store() -> (Vec<Profile>, String) {
    let Ok(blob) = std::fs::read(profiles_file()) else {
        return (Vec::new(), String::new());
    };
    let Ok(plain) = dpapi::unprotect(&blob) else {
        log::warn("账号列表解不开（换了机器或用户？），当成空列表");
        return (Vec::new(), String::new());
    };
    let Ok(v) = serde_json::from_slice::<Value>(&plain) else {
        log::warn("账号列表不是合法 JSON，忽略");
        return (Vec::new(), String::new());
    };
    let active = v.get("active").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let mut items = Vec::new();
    for it in v.get("items").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
        let name = it.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let uuid = it.get("uuid").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let kind = it.get("kind").and_then(|x| x.as_str()).unwrap_or("offline").to_string();
        let rt = it.get("rt").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if name.is_empty() || uuid.is_empty() {
            continue;
        }
        items.push(Profile { name, uuid, kind, rt });
    }
    (items, active)
}

fn save_store(items: &[Profile], active: &str) -> Result<(), String> {
    let arr: Vec<Value> = items
        .iter()
        .map(|p| json!({ "name": p.name, "uuid": p.uuid, "kind": p.kind, "rt": p.rt }))
        .collect();
    let payload = json!({ "active": active, "items": arr }).to_string();
    let blob = dpapi::protect(payload.as_bytes())?;
    let path = profiles_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败：{e}"))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &blob).map_err(|e| format!("写账号列表失败：{e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("改名失败：{e}"))?;
    Ok(())
}

fn upsert(items: &mut Vec<Profile>, p: Profile) {
    if let Some(slot) = items.iter_mut().find(|x| x.uuid == p.uuid) {
        *slot = p;
    } else {
        items.push(p);
    }
}

/// 把某个账号变成「当前账号」：微软账号进 SESSION，离线账号写离线身份文件。
fn apply_active(items: &[Profile], active: &str) {
    let Some(p) = items.iter().find(|x| x.uuid == active) else {
        return;
    };
    if p.kind == "msa" && !p.rt.is_empty() {
        if let Ok(mut g) = SESSION.lock() {
            *g = Some(Session {
                name: p.name.clone(),
                uuid: p.uuid.clone(),
                access_token: String::new(),
                refresh_token: p.rt.clone(),
                expires_at: Instant::now(),
            });
        }
        log::info(&format!("切换到账号 {}（令牌待刷新）", p.name));
    } else {
        if let Ok(mut g) = SESSION.lock() {
            *g = None;
        }
        let doc = json!({ "name": p.name, "uuid": p.uuid });
        let path = offline_file();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, doc.to_string());
        log::info(&format!("切换到离线身份 {}", p.name));
    }
}

/// 登录成功后把账号记进列表，并设为当前。
fn remember_msa(s: &Session) {
    let (mut items, _) = load_store();
    upsert(
        &mut items,
        Profile {
            name: s.name.clone(),
            uuid: s.uuid.clone(),
            kind: "msa".into(),
            rt: s.refresh_token.clone(),
        },
    );
    if let Err(e) = save_store(&items, &s.uuid) {
        log::warn(&format!("账号列表写入失败：{e}"));
    }
}

fn remember_offline(name: &str, uuid: &str) {
    let (mut items, _) = load_store();
    upsert(
        &mut items,
        Profile {
            name: name.to_string(),
            uuid: uuid.to_string(),
            kind: "offline".into(),
            rt: String::new(),
        },
    );
    if let Err(e) = save_store(&items, uuid) {
        log::warn(&format!("账号列表写入失败：{e}"));
    }
}

/// 切换账号：`{ "id": "<uuid>" }`。
pub fn switch(req: &[u8]) -> Value {
    let v: Value = match serde_json::from_slice(req) {
        Ok(v) => v,
        Err(_) => return json!({ "ok": false, "error": "请求格式不对" }),
    };
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let (items, _) = load_store();
    if !items.iter().any(|p| p.uuid == id) {
        return json!({ "ok": false, "error": "这个账号不在列表里" });
    }
    apply_active(&items, &id);
    if let Err(e) = save_store(&items, &id) {
        return json!({ "ok": false, "error": format!("保存失败：{e}") });
    }
    json!({ "ok": true })
}

/// 删除账号：`{ "id": "<uuid>" }`。删的是当前账号时自动退到列表里的下一个。
pub fn remove(req: &[u8]) -> Value {
    let v: Value = match serde_json::from_slice(req) {
        Ok(v) => v,
        Err(_) => return json!({ "ok": false, "error": "请求格式不对" }),
    };
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let (mut items, active) = load_store();
    let before = items.len();
    items.retain(|p| p.uuid != id);
    if items.len() == before {
        return json!({ "ok": false, "error": "这个账号不在列表里" });
    }
    let next = if active == id {
        items.first().map(|p| p.uuid.clone()).unwrap_or_default()
    } else {
        active
    };
    if next.is_empty() {
        if let Ok(mut g) = SESSION.lock() {
            *g = None;
        }
        let path = account_file();
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    } else {
        apply_active(&items, &next);
    }
    if let Err(e) = save_store(&items, &next) {
        return json!({ "ok": false, "error": format!("保存失败：{e}") });
    }
    json!({ "ok": true })
}

/// 启动游戏时用的身份：登录了就正版，否则用用户自己配的离线身份。
// ---- 皮肤头像 --------------------------------------------------------------

/// 标准 base64 解码（Mojang 的 textures 属性是 base64 的 JSON）。
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &c in s.as_bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = val(c)?;
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn skins_file() -> std::path::PathBuf {
    paths::data_root().join("cache").join("skins.json")
}

fn load_skins() -> serde_json::Map<String, Value> {
    let raw = std::fs::read(skins_file()).ok();
    let raw = raw.and_then(|b| {
        let b = b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&b);
        serde_json::from_slice::<Value>(b).ok()
    });
    raw.and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

fn save_skins(map: &serde_json::Map<String, Value>) {
    let path = skins_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_vec(&Value::Object(map.clone())) {
        let _ = std::fs::write(path, text);
    }
}

/// 玩家皮肤地址：Mojang 会话服务器 → `textures` 属性里的 SKIN url。
///
/// 只取地址、不下载图片：页面用 CSS 把 64x64 皮肤里的头部裁出来显示，
/// 这样既不用在 Rust 里写 PNG 解码，也能让浏览器自己缓存图片。
pub fn skin(req: &[u8]) -> Value {
    let v: Value = serde_json::from_slice(req).unwrap_or(Value::Null);
    let mut uuid = v
        .get("uuid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if uuid.is_empty() {
        uuid = identity()["uuid"].as_str().unwrap_or("").to_string();
    }
    let uuid = uuid.replace('-', "");
    if uuid.is_empty() {
        return json!({ "ok": false, "error": "还没有账号" });
    }
    let mut cache = load_skins();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 缓存 7 天：玩家换了皮肤之后最多一周就会自己更新，不用手动清缓存。
    if let Some(hit) = cache.get(&uuid) {
        let fresh = hit
            .get("at")
            .and_then(Value::as_u64)
            .map(|at| now.saturating_sub(at) < 7 * 24 * 3600)
            .unwrap_or(false);
        if let Some(url) = hit.get("url").and_then(Value::as_str).filter(|u| !u.is_empty()) {
            if fresh {
                return json!({ "ok": true, "uuid": uuid, "url": url, "cached": true });
            }
        }
    }
    let url = format!("https://sessionserver.mojang.com/session/minecraft/profile/{uuid}");
    let resp = match crate::net::get_limited(&url, std::time::Duration::from_secs(8), 256 * 1024) {
        Ok(r) if r.status == 200 => r,
        Ok(r) => return json!({ "ok": false, "error": format!("皮肤服务返回 {}", r.status) }),
        Err(e) => return json!({ "ok": false, "error": e }),
    };
    let profile: Value = match serde_json::from_slice(&resp.body) {
        Ok(v) => v,
        Err(_) => return json!({ "ok": false, "error": "皮肤服务返回的内容看不懂" }),
    };
    let encoded = profile
        .get("properties")
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter()
                .find(|p| p.get("name").and_then(Value::as_str) == Some("textures"))
        })
        .and_then(|p| p.get("value"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let decoded = b64_decode(encoded).and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let skin_url = decoded
        .as_ref()
        .and_then(|v| v.get("textures"))
        .and_then(|t| t.get("SKIN"))
        .and_then(|s| s.get("url"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if skin_url.is_empty() {
        return json!({ "ok": false, "error": "这个账号还没有设置皮肤" });
    }
    // Mojang 给的是 http://textures.minecraft.net/...，页面里统一走 https。
    let skin_url = if let Some(rest) = skin_url.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        skin_url
    };
    cache.insert(uuid.clone(), json!({ "url": skin_url, "at": now }));
    save_skins(&cache);
    json!({ "ok": true, "uuid": uuid, "url": skin_url })
}

pub fn identity() -> Value {
    let logged = SESSION
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| (s.name.clone(), s.uuid.clone(), s.access_token.clone())));
    match logged {
        Some((name, uuid, token)) => json!({
            "ok": true,
            "loggedIn": true,
            "type": "msa",
            "name": name,
            "uuid": uuid,
            "token": token,
            "user_type": "msa",
        }),
        None => {
            let o = offline_get();
            json!({
                "ok": true,
                "loggedIn": false,
                "type": "offline",
                "name": o["name"],
                "uuid": o["uuid"],
                "token": "0",
                "user_type": "legacy",
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(d: [u8; 16]) -> String {
        d.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn md5_known_vectors() {
        assert_eq!(hex(md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        // 跨块输入：长度跨过 56 字节的补位边界。
        assert_eq!(
            hex(md5(b"12345678901234567890123456789012345678901234567890123456789012345678901234567890")),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn offline_uuid_matches_vanilla() {
        assert_eq!(uuid_for("Notch"), "b50ad385-829d-3141-a216-7e7d7539ba7f");
        // Player 的 md5 第三组是 5998、变体位是 5，按规定要改成 3998 / 8。
        assert_eq!(uuid_for("Player"), "a01e3843-e521-3998-858a-f459800e4d11");
    }
}
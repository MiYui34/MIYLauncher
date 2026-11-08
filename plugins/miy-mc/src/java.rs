//! Java 环境：发现、按版本挑选、缺了就自动下载。
//!
//! 三件事分开写，是因为它们失败的方式完全不同：
//!
//! - **发现**（`scan`）：机器上可能装了七八个 Java。扫常见安装目录、`JAVA_HOME`、
//!   `PATH` 和启动器自己下过的那份；目录里有 `release` 文件就直接读版本号（快，
//!   不用起进程），没有才跑一次 `java -version`。
//! - **挑选**（`pick`）：按版本 JSON 要求的 Java 主版本挑。要求是"至少"而不是"等于"
//!   （PCL2 的 `GetJavaRequirement` 也是下限），但**优先**要正好这个主版本的：
//!   Java 26 跑 26.3 通常没事，可一旦出事，用户根本分不清是游戏的问题还是 JDK 的问题。
//! - **下载**（`install`）：照 PCL2 的做法问 Mojang 要运行时清单
//!   （`piston-meta.mojang.com/v1/products/java-runtime/<sha>/all.json`），按版本挑一个
//!   `java-runtime-*` 组件，把它 manifest 里的文件逐个下载下来，装到
//!   `<数据目录>\java\<组件名>\` —— 不写进 `%APPDATA%\.minecraft\runtime`，
//!   启动器的所有文件都待在 exe 旁边。
//!
//! 下载完每个文件都校验 sha1：Mojang 的 manifest 就给了哈希，不校验的话，
//! 镜像返回一个 200 的错误页也能被当成"下载成功"，最后报的错会是"启动 java 失败"。

use miy_abi::MiyHostV1;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Mutex;

use crate::{call_json, fetch_all, Item};

/// 一个可用的 Java。
#[derive(Clone, Debug)]
pub struct Java {
    /// `java.exe` 的完整路径。
    pub path: String,
    /// 主版本（1.8.0_401 → 8，25.0.1 → 25）。
    pub major: u32,
    /// 从哪儿找到的，给日志和界面用。
    pub source: String,
}

impl Java {
    pub fn to_json(&self) -> Value {
        json!({ "path": self.path, "major": self.major, "source": self.source })
    }
}

/// 扫一次要花几十毫秒（主要是 `reg`/目录遍历 + 偶尔起一次 `java -version`），
/// 缓存起来；下载完新 Java 之后调 `invalidate()` 让它重新扫。
static CACHE: Mutex<Option<Vec<Java>>> = Mutex::new(None);

pub fn invalidate() {
    if let Ok(mut g) = CACHE.lock() {
        *g = None;
    }
}

/// Mojang 的 Java 运行时清单。第二个是 BMCLAPI 镜像，主站不通时用。
const RUNTIME_ALL: &str = "https://piston-meta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";
const RUNTIME_ALL_MIRROR: &str = "https://bmclapi2.bangbang93.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";

/// PCL2 (#3827) 跳过的三个文件：在 manifest 里大量重复，下了也只是白占空间。
const JUNK_SHA1: [&str; 3] = [
    "12976a6c2b227cbac58969c1455444596c894656",
    "c80e4bab46e34d02826eab226a4441d0970f2aba",
    "84d2102ad171863db04e7ee22a259d1f6c5de4a5",
];

/// 某个版本需要什么 Java。
///
/// 规则照 PCL2 的 `GetJavaRequirement`：`javaVersion.majorVersion` 是 Mojang 自己声明的
/// 下限（26.x 的 25 就来自这里）；老版本没有这个字段，就按版本号/发布时间推。
/// 返回 (主版本下限, 推荐组件名)。
pub fn required(v: &Value, id: &str) -> (u32, Option<String>) {
    let jv = v.get("javaVersion");
    let declared = jv
        .and_then(|j| j.get("majorVersion"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let component = jv
        .and_then(|j| j.get("component"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // 老版本靠版本号推：1.20.5+ / 1.21+ 要 21，1.18+ 要 17，1.17+ 要 16，其余 8。
    let vanilla = crate::version_major(id); // 1.x → x；26.x 之类 → 99
    let fallback = if vanilla >= 21 {
        21
    } else if vanilla >= 18 {
        17
    } else if vanilla >= 17 {
        16
    } else {
        8
    };
    // Mojang 只在 1.17+ 的 JSON 里写 majorVersion，写了的（16/17/21/25…）比推断更准。
    let need = if declared >= 16 { declared } else { fallback };
    (need, component)
}

/// 扫机器上所有能找到的 Java。
pub fn scan(h: &MiyHostV1) -> Vec<Java> {
    if let Some(v) = CACHE.lock().ok().and_then(|g| g.clone()) {
        return v;
    }
    let list = scan_uncached(h);
    if let Ok(mut g) = CACHE.lock() {
        *g = Some(list.clone());
    }
    list
}

fn scan_uncached(h: &MiyHostV1) -> Vec<Java> {
    let mut out: Vec<Java> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    // 1. 启动器自己下过的（最可靠：版本和应用是一起装的）。
    if let Some(dir) = data_dir(h) {
        let java_root = format!("{dir}\\java");
        if let Ok(rd) = std::fs::read_dir(&java_root) {
            for e in rd.flatten() {
                add(&mut out, &mut seen, &e.path().to_string_lossy(), "启动器下载");
            }
        }
    }

    // 2. `JAVA_HOME`。
    if let Ok(home) = std::env::var("JAVA_HOME") {
        add(&mut out, &mut seen, &home, "JAVA_HOME");
    }

    // 3. 常见安装目录（只读扫描，不往里面写任何东西）。
    let mut roots: Vec<String> = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                roots.push(if var == "LOCALAPPDATA" {
                    format!("{v}\\Programs")
                } else {
                    v
                });
            }
        }
    }
    roots.push("C:\\ProgramData".to_string());
    for root in &roots {
        scan_tree(Path::new(root), 2, &mut out, &mut seen);
    }

    // 4. `PATH` 里的 java.exe —— 很多人只把它加进 PATH。
    if let Ok(path) = std::env::var("PATH") {
        for part in path.split(';') {
            let p = part.trim().trim_matches('"');
            if p.is_empty() {
                continue;
            }
            let exe = format!("{p}\\java.exe");
            if Path::new(&exe).is_file() {
                add_exe(&mut out, &mut seen, &exe, "PATH");
                break;
            }
        }
    }

    // 主版本高的排前面；同版本时"启动器下载"优先（它一定匹配得上）。
    out.sort_by(|a, b| {
        b.major
            .cmp(&a.major)
            .then(rank(a).cmp(&rank(b)))
            .then(a.path.cmp(&b.path))
    });
    out
}

fn rank(j: &Java) -> u8 {
    if j.source == "启动器下载" {
        0
    } else if j.source == "JAVA_HOME" {
        1
    } else {
        2
    }
}

/// 深度最多 `depth` 层地找 `bin\java.exe`。
///
/// 只看两层是因为安装器都这样摆：`Zulu\zulu-25\bin`、`Eclipse Adoptium\jdk-21.x\bin`、
/// `Microsoft\jdk-21.x-hotspot\bin`。再深就是游戏自己带的 `.minecraft\runtime`，
/// 那属于"别人的安装"，不该由我们认领。
fn scan_tree(dir: &Path, depth: usize, out: &mut Vec<Java>, seen: &mut Vec<String>) {
    if depth == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        // 只往名字像 Java 的目录里钻，否则两层扫 Program Files 会把整个磁盘翻一遍。
        let looks_java = name.contains("java")
            || name.contains("jdk")
            || name.contains("jre")
            || name.contains("zulu")
            || name.contains("adoptium")
            || name.contains("temurin")
            || name.contains("corretto")
            || name.contains("liberica")
            || name.contains("semeru")
            || name.contains("bellsoft")
            || name.contains("microsoft")
            || name.contains("jetbrains")
            || name.contains("jbr")
            || name.contains("graalvm");
        if !looks_java {
            continue;
        }
        add(out, seen, &p.to_string_lossy(), "安装目录");
        // `<vendor>\<version>\bin\java.exe`：再下一层看看。
        scan_tree(&p, depth - 1, out, seen);
    }
}

fn add(out: &mut Vec<Java>, seen: &mut Vec<String>, home: &str, source: &str) {
    let home = home.trim().trim_matches('"');
    // `JAVA_HOME` 经常带着结尾的反斜杠（`...\zulu-25\`），拼出来就是 `...\zulu-25\\bin`。
    // 路径能用，但去重会认成两个，界面和日志里也别扭。
    let home = home.trim_end_matches(['\\', '/']);
    if home.is_empty() {
        return;
    }
    if home.to_lowercase().ends_with("java.exe") {
        add_exe(out, seen, home, source);
        return;
    }
    for cand in [
        format!("{home}\\bin\\java.exe"),
        format!("{home}\\jre\\bin\\java.exe"),
    ] {
        if Path::new(&cand).is_file() {
            add_exe(out, seen, &cand, source);
            return;
        }
    }
}

fn add_exe(out: &mut Vec<Java>, seen: &mut Vec<String>, exe: &str, source: &str) {
    let key = exe.to_lowercase();
    if seen.iter().any(|s| s == &key) {
        return;
    }
    seen.push(key);
    let major = major_of(exe);
    if major == 0 {
        return; // 跑不起来（缺 dll、被拦、架构不对），当没装
    }
    out.push(Java { path: exe.to_string(), major, source: source.to_string() });
}

/// `java.exe` 的主版本。先读同目录的 `release`（不用起进程），读不到再问 JVM。
fn major_of(exe: &str) -> u32 {
    let path = Path::new(exe);
    if let Some(home) = path.parent().and_then(Path::parent) {
        if let Some(m) = release_major(home) {
            return m;
        }
    }
    probe_major(exe)
}

fn release_major(home: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(home.join("release")).ok()?;
    for line in text.lines() {
        // JAVA_VERSION="25.0.1"
        if let Some(rest) = line.trim().strip_prefix("JAVA_VERSION=") {
            let v = rest.trim().trim_matches('"');
            let m = major_from(v);
            if m > 0 {
                return Some(m);
            }
        }
    }
    None
}

/// 跑一次 `java -version`。失败返回 0。
///
/// `-version` 把版本写到 stderr（十几年来都这样），stdout 和 stderr 都要看。
fn probe_major(exe: &str) -> u32 {
    let Ok(out) = std::process::Command::new(exe).arg("-version").output() else {
        return 0;
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // 找 `version "25.0.1"` 里的那一段。
    if let Some(i) = text.find("version \"") {
        let rest = &text[i + 9..];
        if let Some(j) = rest.find('"') {
            return major_from(&rest[..j]);
        }
    }
    // 老 JVM 有时只写 `java version 1.8.0_401`（没有引号）。
    for line in text.lines() {
        if line.contains(" version ") {
            if let Some(tok) = line.split(" version ").nth(1).and_then(|s| s.split_whitespace().next()) {
                return major_from(tok);
            }
        }
    }
    0
}

/// `1.8.0_401` → 8；`25.0.1` → 25；`8u51` → 8。
fn major_from(v: &str) -> u32 {
    let parts: Vec<u32> = v
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<u32>().ok())
        .collect();
    match parts.as_slice() {
        [1, minor, ..] => *minor,
        [major, ..] => *major,
        [] => 0,
    }
}

/// 从候选里挑：先要**正好**这个主版本的。
///
/// 没有正好的时候分两种情形（和 PCL2 的版本范围一个意思）：
/// - 新版（要求 Java 16+，PCL2 的 `AtLeast`）往高了挑也行，选最小的那个更高的；
/// - 老版（要求 Java 8，PCL2 是 `< 9` 的上界）**不能用更高的** —— 1.7.10 这种版本
///   拿 Java 21 去跑通常直接崩在 LWJGL 上，宁可去下 Mojang 的 jre-legacy。
///
/// 不要更低的：Java 太老的报错是 `UnsupportedClassVersionError`，用户看到的却是
/// 游戏窗口一闪而过，根本查不到原因。
pub fn pick(list: &[Java], need: u32) -> Option<Java> {
    let mut exact: Vec<&Java> = list.iter().filter(|j| j.major == need).collect();
    exact.sort_by(|a, b| rank(a).cmp(&rank(b)).then(a.path.cmp(&b.path)));
    if let Some(j) = exact.first() {
        return Some((*j).clone());
    }
    if need < 16 {
        return None;
    }
    let mut higher: Vec<&Java> = list.iter().filter(|j| j.major > need).collect();
    higher.sort_by(|a, b| a.major.cmp(&b.major).then(rank(a).cmp(&rank(b))));
    higher.first().map(|j| (*j).clone())
}

/// 挑一个 Java；挑不到就下（`allow_download` 关掉时直接报错）。
pub fn ensure(
    h: &MiyHostV1,
    need: u32,
    component: Option<&str>,
    allow_download: bool,
    notes: &mut Vec<String>,
) -> Result<Java, String> {
    let picked = pick(&scan(h), need);
    if let Some(j) = picked {
        if j.major > need {
            notes.push(format!(
                "这台机器上没有 Java {need}，改用 Java {}（{}）",
                j.major, j.source
            ));
        }
        return Ok(j);
    }
    if !allow_download {
        return Err(format!(
            "没找到 Java {need}，而「自动下载 Java」是关着的。请安装 Java {need}，或在设置里填 java.exe 的路径"
        ));
    }
    install(h, need, component, notes)
}

/// 下载 Mojang 提供的 Java 运行时并装到 `<数据目录>\java\<组件>\`。
pub fn install(
    h: &MiyHostV1,
    need: u32,
    component: Option<&str>,
    notes: &mut Vec<String>,
) -> Result<Java, String> {
    let all = get_json(h, &[RUNTIME_ALL, RUNTIME_ALL_MIRROR])?;
    let (name, manifest_url, version) = choose(&all, need, component)
        .ok_or_else(|| format!("Mojang 没有提供 Java {need} 的运行时，请手动装一个再启动"))?;

    let data = data_dir(h).ok_or("拿不到数据目录")?;
    let base = format!("{data}\\java\\{name}");
    let exe = format!("{base}\\bin\\java.exe");

    // 之前下过一半：目录还在但 java.exe 没落地的，重下。
    let already = Path::new(&exe).is_file() && major_of(&exe) >= need;
    if !already {
        let msg = format!("没有可用的 Java {need}，正在下载 Mojang 的 {name}（Java {version}）");
        log_line(h, &msg);
        notes.push(msg);

        let manifest_mirror = mirror(&manifest_url, "piston-meta.mojang.com");
        let manifest = get_json(h, &[&manifest_url, &manifest_mirror])?;
        let files = collect_files(&manifest, &base)?;
        if files.is_empty() {
            return Err("Java 的 manifest 里没有可下载的文件".into());
        }

        let items: Vec<Item> = files
            .iter()
            .map(|f| Item {
                path: f.0.clone(),
                url: f.1.clone(),
                size: f.2,
                sha1: Some(f.3.clone()),
            })
            .collect();
        let got = fetch_all(h, &base, &items, 16, 24, None, None);
        if let Some(e) = got.errors.first() {
            let _ = std::fs::remove_dir_all(&base);
            return Err(format!("下载 Java 失败：{e}"));
        }

        // sha1 校验：镜像 200 也可能是错误页，Mojang 的 manifest 里给了哈希，就用它。
        let mut bad: Vec<&(String, String, u64, String)> = Vec::new();
        for f in &files {
            let p = format!("{base}\\{}", f.0);
            match std::fs::read(&p) {
                Ok(bytes) if sha1_hex(&bytes) == f.3 => {}
                _ => bad.push(f),
            }
        }
        if !bad.is_empty() {
            for f in &bad {
                let _ = fetch_all(
                    h,
                    &base,
                    &[Item {
                        path: f.0.clone(),
                        url: f.1.clone(),
                        size: f.2,
                        sha1: Some(f.3.clone()),
                    }],
                    1,
                    1,
                    None,
                    None,
                );
            }
            let mut still = Vec::new();
            for f in &files {
                let p = format!("{base}\\{}", f.0);
                match std::fs::read(&p) {
                    Ok(bytes) if sha1_hex(&bytes) == f.3 => {}
                    _ => still.push(f.0.clone()),
                }
            }
            if !still.is_empty() {
                let _ = std::fs::remove_dir_all(&base);
                return Err(format!(
                    "Java 下载后校验失败（{} 个文件），已清掉重来。请检查网络或换个镜像",
                    still.len()
                ));
            }
        }
    }

    if !Path::new(&exe).is_file() {
        let _ = std::fs::remove_dir_all(&base);
        return Err(format!("Java 下载完了却没找到 {exe}"));
    }
    let major = major_of(&exe);
    if major < need {
        return Err(format!(
            "下载到的 Java 是 {major}，但这个版本要 {need}（{}）",
            if major == 0 { "甚至跑不起来".to_string() } else { "版本不对".to_string() }
        ));
    }
    invalidate();
    let done = format!("已装好 Java {major}：{base}");
    log_line(h, &done);
    notes.push(done);
    Ok(Java { path: exe, major, source: "启动器下载".to_string() })
}

/// 从 Mojang 的运行时清单里挑一个组件。
///
/// 优先用版本 JSON 点名的组件（26.3 的 `java-runtime-epsilon`），否则挑主版本最接近
/// 下限的那个；都不满足就返回 None（宁可报错，也不下个跑不起来的 Java）。
fn choose(all: &Value, need: u32, component: Option<&str>) -> Option<(String, String, String)> {
    let w = all.get("windows-x64")?.as_object()?;
    let mut cands: Vec<(String, String, String, u32)> = Vec::new();
    for (name, arr) in w {
        if name == "minecraft-java-exe" {
            continue; // 只有 java.exe，不是完整运行时
        }
        let Some(entry) = arr.as_array().and_then(|a| a.first()) else { continue };
        let url = entry
            .get("manifest")
            .and_then(|m| m.get("url"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let ver = entry
            .get("version")
            .and_then(|v| v.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if url.is_empty() || ver.is_empty() {
            continue;
        }
        cands.push((name.clone(), url.to_string(), ver.to_string(), major_from(ver)));
    }
    if let Some(c) = component {
        if let Some(hit) = cands.iter().find(|x| x.0 == c && x.3 >= need) {
            return Some((hit.0.clone(), hit.1.clone(), hit.2.clone()));
        }
    }
    // 正好等于下限的 > 更高的。26.x 要 25，机器上有 25 就不要下 26。
    cands.retain(|c| c.3 >= need);
    cands.sort_by_key(|c| (c.3 != need, c.3));
    cands.first().map(|c| (c.0.clone(), c.1.clone(), c.2.clone()))
}

/// 把 manifest 摊平成 (相对路径, url, 大小, sha1)。
fn collect_files(manifest: &Value, base: &str) -> Result<Vec<(String, String, u64, String)>, String> {
    let files = manifest
        .get("files")
        .and_then(Value::as_object)
        .ok_or("Java 的 manifest 里没有 files")?;
    let mut out = Vec::new();
    for (name, info) in files {
        let typ = info.get("type").and_then(Value::as_str).unwrap_or("file");
        if typ == "directory" {
            let _ = std::fs::create_dir_all(format!("{base}\\{name}"));
            continue;
        }
        let Some(raw) = info.get("downloads").and_then(|d| d.get("raw")) else { continue };
        let (Some(url), Some(sha1), Some(size)) = (
            raw.get("url").and_then(Value::as_str),
            raw.get("sha1").and_then(Value::as_str),
            raw.get("size").and_then(Value::as_u64),
        ) else {
            continue;
        };
        if JUNK_SHA1.contains(&sha1) {
            continue;
        }
        out.push((name.clone(), url.to_string(), size, sha1.to_lowercase()));
    }
    Ok(out)
}

fn get_json(h: &MiyHostV1, urls: &[&str]) -> Result<Value, String> {
    let mut last = String::from("没有可用的地址");
    for url in urls {
        let req = json!({ "url": url, "timeout_ms": 60_000u64, "max_bytes": 8 * 1024 * 1024 });
        match call_json(h, "miy.net.get", &req) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                let text = v.get("text").and_then(Value::as_str).unwrap_or("");
                match serde_json::from_str::<Value>(text) {
                    Ok(j) => return Ok(j),
                    Err(e) => last = format!("{url} 返回的不是 JSON：{e}"),
                }
            }
            Ok(v) => {
                last = v
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("请求失败")
                    .to_string()
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn mirror(url: &str, host: &str) -> String {
    url.replace(host, "bmclapi2.bangbang93.com")
}

fn data_dir(h: &MiyHostV1) -> Option<String> {
    let info = call_json(h, "miy.app.info", &json!({})).ok()?;
    info.get("dataRoot").and_then(Value::as_str).map(str::to_string)
}

fn log_line(h: &MiyHostV1, line: &str) {
    let payload = serde_json::to_vec(&json!({ "line": format!("[启动] {line}") })).unwrap_or_default();
    unsafe { h.emit_str("mc.log", &payload) };
}

/// SHA-1。为了校验 Java 文件自己写一个，比拉一个加密库进来便宜：
/// 启动器要多带 100 KB 才能 sha1，不值得。
pub(crate) fn sha1_hex(data: &[u8]) -> String {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let bits = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_known_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn parses_majors() {
        assert_eq!(major_from("1.8.0_401"), 8);
        assert_eq!(major_from("25.0.1"), 25);
        assert_eq!(major_from("8u51"), 8);
        assert_eq!(major_from("17"), 17);
        assert_eq!(major_from(""), 0);
    }

    fn fake(major: u32, source: &str) -> Java {
        Java { path: format!("C:\\java{major}\\bin\\java.exe"), major, source: source.into() }
    }

    #[test]
    fn pick_prefers_exact_major() {
        let list = [fake(21, "安装目录"), fake(25, "JAVA_HOME")];
        assert_eq!(pick(&list, 25).unwrap().major, 25);
        assert_eq!(pick(&list, 21).unwrap().major, 21);
    }

    #[test]
    fn pick_allows_higher_only_for_modern() {
        // 新版：要求 17，只有 21 —— 可以用（PCL2 的 AtLeast 范围）。
        let list = [fake(21, "安装目录")];
        assert_eq!(pick(&list, 17).unwrap().major, 21);
        // 老版：要求 8，只有 21 —— 不能用，应去下 jre-legacy。
        assert!(pick(&list, 8).is_none());
        // 一个正好的 8 就还是用它。
        let list = [fake(21, "安装目录"), fake(8, "启动器下载")];
        assert_eq!(pick(&list, 8).unwrap().major, 8);
    }
}

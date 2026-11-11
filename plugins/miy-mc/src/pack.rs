//! Modrinth 整合包（`.mrpack`）的解析与解压。
//!
//! 格式定义见 <https://docs.modrinth.com/docs/modpacks/format_definition>：
//! 一个 zip，根上 `modrinth.index.json` 写着游戏版本、加载器版本和要下载的文件，
//! `overrides/`（双端共用）与 `client-overrides/`（仅客户端）是直接铺进实例目录的散文件。
//!
//! 这个模块只负责"把包读懂、把散文件铺开"，不知道安装是什么；
//! 装原版、装加载器、下载 `files[]` 都由 `lib.rs` 的 `service_modpack_install` 编排。

use super::{safe_rel, zip_list, zip_read, ZipEntry};
use serde_json::Value;

/// 一个整合包的全部元信息。
#[derive(Debug)]
pub struct Pack {
    /// 包名（来自 `name`，用来做版本目录名）。
    pub name: String,
    /// 依赖的 Minecraft 版本，如 `1.21.1`。
    pub mc: String,
    /// 加载器：`fabric` / `quilt` / `forge` / `neoforge`，空串表示纯原版。
    pub loader: String,
    /// 加载器版本号。
    pub loader_ver: String,
    /// 需要按 URL 拉取的文件。
    pub files: Vec<PackFile>,
}

/// `files[]` 里的一条：下到实例目录的哪个位置、从哪下、多大、sha1 多少。
#[derive(Debug)]
pub struct PackFile {
    pub path: String,
    pub url: String,
    pub size: u64,
    pub sha1: Option<String>,
}

/// 清单里的加载器键 → 我们内部用的名字。顺序即优先级（一个包不该同时有俩，
/// 但真遇到了就按 Modrinth 自己的习惯取第一个）。
const LOADERS: &[(&str, &str)] = &[
    ("fabric-loader", "fabric"),
    ("quilt-loader", "quilt"),
    ("neoforge", "neoforge"),
    ("forge", "forge"),
];

/// 读 `modrinth.index.json`。失败时给的是"用户能看懂哪一步不对"的话。
pub fn read_index(bytes: &[u8]) -> Result<Pack, String> {
    let entries = zip_list(bytes)?;
    let idx = entries
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case("modrinth.index.json"))
        .ok_or("这个压缩包里没有 modrinth.index.json，不像是 Modrinth 整合包")?;
    let raw = zip_read(bytes, idx)?;
    let v: Value = serde_json::from_slice(&raw).map_err(|e| format!("整合包清单不是合法 JSON：{e}"))?;

    let deps = v.get("dependencies").cloned().unwrap_or(Value::Null);
    let mc = deps
        .get("minecraft")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if mc.is_empty() {
        return Err("整合包清单里没写 Minecraft 版本".into());
    }
    let (loader, loader_ver) = LOADERS
        .iter()
        .find_map(|(key, name)| {
            deps.get(*key).and_then(Value::as_str).map(|lv| (*name, lv))
        })
        .map(|(n, lv)| (n.to_string(), lv.to_string()))
        .unwrap_or_default();

    let name = v
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{mc} 整合包"));

    let mut files = Vec::new();
    if let Some(arr) = v.get("files").and_then(Value::as_array) {
        for f in arr {
            // 只给服务端的文件（`env.client == "unsupported"`）装了也没用。
            let env_client = f
                .get("env")
                .and_then(|e| e.get("client"))
                .and_then(Value::as_str)
                .unwrap_or("required");
            if env_client.eq_ignore_ascii_case("unsupported") {
                continue;
            }
            let Some(rel) = f.get("path").and_then(Value::as_str) else {
                continue;
            };
            // 包里给的路径是 `/` 分隔的；`safe_rel` 顺手挡住 `..\..\Windows`。
            let Some(rel) = safe_rel(rel) else {
                continue;
            };
            let Some(url) = f
                .get("downloads")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty())
            else {
                // 规范允许 downloads 为空（让启动器自己找），我们没那个能力，跳过。
                continue;
            };
            let size = f.get("fileSize").and_then(Value::as_u64).unwrap_or(0);
            let sha1 = f
                .get("hashes")
                .and_then(|h| h.get("sha1"))
                .and_then(Value::as_str)
                .map(str::to_string);
            files.push(PackFile {
                path: rel.replace('\\', "/"),
                url: url.to_string(),
                size,
                sha1,
            });
        }
    }

    Ok(Pack {
        name,
        mc,
        loader,
        loader_ver,
        files,
    })
}

/// 把 `overrides/` 与 `client-overrides/` 铺进实例目录。
///
/// 先铺 `overrides/` 再铺 `client-overrides/`：同一路径以客户端侧为准，
/// 这是规范要求的覆盖顺序。返回写了多少个文件。
pub fn extract_overrides(bytes: &[u8], dest: &str) -> Result<usize, String> {
    let entries = zip_list(bytes)?;
    let mut written = 0usize;
    for prefix in ["overrides/", "client-overrides/"] {
        for e in &entries {
            let rel = e.name.replace('\\', "/");
            let Some(rest) = rel.strip_prefix(prefix) else {
                continue;
            };
            if rest.is_empty() || rest.ends_with('/') {
                continue;
            }
            let Some(rel_out) = safe_rel(rest) else {
                continue;
            };
            let Ok(data) = zip_read(bytes, e) else {
                continue;
            };
            let target = std::path::Path::new(dest).join(&rel_out);
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::write(&target, data).is_ok() {
                written += 1;
            }
        }
    }
    Ok(written)
}

/// 包里有没有 `modrinth.index.json` —— 版本页拖进来一个 zip 时先用它判断格式。
pub fn is_mrpack(bytes: &[u8]) -> bool {
    zip_list(bytes)
        .map(|es| {
            es.iter()
                .any(|e: &ZipEntry| e.name.eq_ignore_ascii_case("modrinth.index.json"))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手搓一个 stored（不压缩）zip，够测解析用。
    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in files {
            let off = out.len();
            out.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]);
            out.extend_from_slice(&[20, 0]);
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(&[0, 0, 0, 0]); // time/date
            out.extend_from_slice(&crc32(data).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            central.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
            central.extend_from_slice(&[20, 0, 20, 0]);
            central.extend_from_slice(&[0, 0, 0, 0]);
            central.extend_from_slice(&[0, 0, 0, 0]);
            central.extend_from_slice(&crc32(data).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(off as u32).to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len();
        out.extend_from_slice(&central);
        out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&(cd_off as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    fn index(extra: &str) -> String {
        format!(
            r#"{{"formatVersion":1,"game":"minecraft","versionId":"1.0.0",
"name":"{extra}","dependencies":{{"minecraft":"1.21.1","fabric-loader":"0.16.5"}},
"files":[{{"path":"mods/sodium.jar","hashes":{{"sha1":"abc"}},"fileSize":123,"downloads":["https://cdn.modrinth.com/a.jar"]}},
{{"path":"mods/serveronly.jar","fileSize":9,"downloads":["https://cdn.modrinth.com/b.jar"],"env":{{"client":"unsupported"}}}}]}}"#
        )
    }

    #[test]
    fn reads_index_and_drops_server_only_files() {
        let bytes = zip_of(&[("modrinth.index.json", index("测试包").as_bytes())]);
        let p = read_index(&bytes).unwrap();
        assert_eq!(p.mc, "1.21.1");
        assert_eq!(p.loader, "fabric");
        assert_eq!(p.loader_ver, "0.16.5");
        assert_eq!(p.files.len(), 1, "server-only 的文件不该进下载队列");
        assert_eq!(p.files[0].path, "mods/sodium.jar");
        assert_eq!(p.files[0].sha1.as_deref(), Some("abc"));
    }

    #[test]
    fn rejects_zip_without_index() {
        let bytes = zip_of(&[("mods/x.jar", b"x")]);
        let e = read_index(&bytes).unwrap_err();
        assert!(e.contains("modrinth.index.json"), "{e}");
        assert!(!is_mrpack(&bytes));
    }

    #[test]
    fn overrides_are_extracted_and_sanitized() {
        let bytes = zip_of(&[
            ("modrinth.index.json", index("包").as_bytes()),
            ("overrides/config/a.json", b"{}"),
            ("overrides/../evil.txt", b"nope"),
            ("client-overrides/options.txt", b"fov:80"),
        ]);
        let dir = std::env::temp_dir().join(format!("miy-pack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let n = extract_overrides(&bytes, dir.to_str().unwrap()).unwrap();
        assert_eq!(n, 2, "越界的 ../evil.txt 必须被丢掉");
        assert!(dir.join("config/a.json").is_file());
        assert!(dir.join("options.txt").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn neoforge_dependency_is_recognized() {
        let idx = r#"{"name":"n","dependencies":{"minecraft":"1.21.1","neoforge":"21.1.80"},"files":[]}"#;
        let bytes = zip_of(&[("modrinth.index.json", idx.as_bytes())]);
        let p = read_index(&bytes).unwrap();
        assert_eq!(p.loader, "neoforge");
        assert_eq!(p.loader_ver, "21.1.80");
    }
}

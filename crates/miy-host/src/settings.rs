//! 设置系统（architecture.md §9）——插件只交 schema，UI 由宿主统一渲染。
//!
//! 为什么不让插件自己画设置页：一旦让插件画，每个插件的设置页长得都不一样，
//! 而且每加一个设置项都要写一遍 HTML + 校验 + 存盘。scheme 化之后这三件事
//! 宿主只做一次，插件作者只需要在 manifest 里写字段。
//!
//! 三层覆盖 `default < 全局 < 实例`，v1 只实现全局层（实例层等实例功能落地再接）。
//! 存盘时**整个文件原样保留**：插件卸载/降级后它的旧值还在，回滚不丢配置。

use serde_json::{json, Map, Value};
use std::sync::Mutex;

use crate::bus;
use crate::paths;

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// 一个设置项。所有都是可选的，缺省即"不限制"。
#[derive(Clone)]
pub struct Field {
    /// 全局唯一的键，约定为 `<owner>.<name>`。
    pub key: String,
    /// 谁贡献的：插件 id 或 `core`。
    pub owner: String,
    pub title: String,
    /// `bool` / `int` / `float` / `string` / `enum`。
    pub kind: String,
    pub default: Value,
    pub choices: Vec<Value>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub group: String,
    pub order: i64,
    pub description: String,
    /// 界面用什么控件画：空 = 按类型默认，`slider` = 滑杆。
    pub control: String,
    /// 滑杆的步长（内存这类整数用它对齐到 512）。
    pub step: Option<f64>,
}

struct State {
    /// settings.json 的**完整**内容，包括我们不认识的字段。
    doc: Value,
    fields: Vec<Field>,
}

fn with_state<T>(f: impl FnOnce(&mut State) -> T) -> Option<T> {
    let mut g = STATE.lock().ok()?;
    let st = g.as_mut()?;
    Some(f(st))
}

/// 宿主自己的设置项。插件贡献的键不能叫 `core.*`（合并时会给插件加前缀）。
fn core_schema() -> Value {
    json!({
        "log.level": {
            "type": "enum",
            "title": "日志级别",
            "description": "debug 会把每次跨边界调用都记下来，平时别开。",
            "enum": ["error", "warn", "info", "debug"],
            "default": "info",
            "group": "核心",
            "order": 0
        },
        "ui.density": {
            "type": "enum",
            "title": "界面密度",
            "description": "紧凑模式适合小屏和笔记本。",
            "enum": ["comfortable", "compact"],
            "default": "comfortable",
            "group": "核心",
            "order": 1
        },
        "ui.intro": {
            "type": "bool",
            "title": "启动动画",
            "description": "启动时先播一段 5 秒的品牌动画，播完进入主界面。关掉后直接进主界面。",
            "default": true,
            "group": "核心",
            "order": 2
        },
        // Microsoft 客户端 ID 与登录端点故意**都不在这里**：它们是凭据/技术参数，
        // 不是偏好。放进设置就会被设置页渲染、被截图带出去。宿主从
        // `<data>/config/auth.json` 读（见 `paths::auth_file`、`accounts::client_id`
        // 与 `accounts::authority`）。
    })
}

pub fn init() {
    let doc = load_doc();
    let mut fields = Vec::new();
    parse_fields("core", &core_schema(), &mut fields);

    if let Ok(mut g) = STATE.lock() {
        *g = Some(State { doc, fields });
    }
}

fn config_path() -> std::path::PathBuf {
    paths::config_file()
}

/// 读不上就当作空文档——**绝不**因为一个坏文件打不开启动器。
fn load_doc() -> Value {
    let path = config_path();
    let mut doc = match std::fs::read(&path) {
        Ok(bytes) if !bytes.is_empty() => {
            // 剥 BOM：Windows PowerShell 5.1 的 `Set-Content -Encoding utf8`、
            // 记事本的「UTF-8」都写 BOM，serde_json 会在第 1 列直接报
            // `expected value`，结果是用户手改过的设置被整份丢弃（还只留一条 warn）。
            let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
            match serde_json::from_slice::<Value>(body) {
                Ok(v) if v.is_object() => v,
                Ok(_) => {
                    crate::log::warn("settings.json 不是对象，已忽略其内容");
                    json!({})
                }
                Err(e) => {
                    crate::log::warn(&format!("settings.json 解析失败（{e}），保留原文件不用"));
                    json!({})
                }
            }
        }
        _ => json!({}),
    };

    // 老版本把设置放在 <data>/config.json。把新文件里**没有**的键补进来，
    // 补完就把老文件改名，免得每次启动都重来一遍。
    let legacy = paths::legacy_config_file();
    if legacy.exists() {
        let old = std::fs::read(&legacy)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        if let Some(old) = old {
            let merged = merge_missing_values(&mut doc, &old);
            if merged > 0 {
                crate::log::info(&format!("从老配置 {} 补进了 {merged} 项设置", legacy.display()));
                let _ = save(&doc);
            }
        }
        // 已经搬过（或者搬不动）都改名，避免下次再撞：信息已经在新文件里了。
        let done = legacy.with_extension("json.migrated");
        if std::fs::rename(&legacy, &done).is_err() {
            let _ = std::fs::remove_file(&legacy);
        }
    }

    doc
}

/// 把 `old` 里 `doc` 没有的设置键补进 `doc`，返回补了几项。
fn merge_missing_values(doc: &mut Value, old: &Value) -> usize {
    let Some(old_vals) = old.get("values").and_then(|v| v.as_object()) else {
        return 0;
    };
    if !doc.is_object() {
        *doc = json!({});
    }
    let root = doc.as_object_mut().expect("上面刚保证过是对象");
    let vals = root
        .entry("values")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(vals) = vals.as_object_mut() else {
        return 0;
    };
    let mut n = 0;
    for (k, v) in old_vals {
        if !vals.contains_key(k) {
            vals.insert(k.clone(), v.clone());
            n += 1;
        }
    }
    n
}

/// 原子写：先写临时文件再 rename，断电时最多丢这一次改动，不会留下半个 JSON。
fn save(doc: &Value) -> Result<(), String> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建配置目录失败: {e}"))?;
    }
    let text = serde_json::to_vec_pretty(doc).map_err(|e| format!("序列化设置失败: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &text).map_err(|e| format!("写设置失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("替换设置失败: {e}"))?;
    Ok(())
}

// ---- schema 合并 -----------------------------------------------------------

/// 插件加载后调用：把 `manifest.contributes["settings.schema"]` 并进来。
///
/// 支持两种写法，作者怎么顺手怎么写：
///
/// ```json
/// "settings.schema": { "renderer": { "type": "enum", "enum": ["auto","gl"], "default": "auto" } }
/// "settings.schema": [ { "key": "renderer", "type": "enum", "enum": ["auto","gl"] } ]
/// ```
pub fn contribute(owner: &str, contributes: &Value) {
    let Some(schema) = contributes.get("settings.schema") else {
        return;
    };
    let mut parsed = Vec::new();
    parse_fields(owner, schema, &mut parsed);
    if parsed.is_empty() {
        return;
    }
    let keys: Vec<String> = parsed.iter().map(|f| f.key.clone()).collect();

    let _ = with_state(|st| {
        // 重复加载同一个插件时，先清掉它上次贡献的项，避免越叠越多。
        st.fields.retain(|f| f.owner != owner);
        st.fields.extend(parsed);
        st.fields.sort_by(|a, b| {
            a.group
                .cmp(&b.group)
                .then(a.order.cmp(&b.order))
                .then(a.key.cmp(&b.key))
        });
    });
    crate::log::debug(&format!("{owner} 贡献了 {} 个设置项", keys.len()));
}

fn parse_fields(owner: &str, schema: &Value, out: &mut Vec<Field>) {
    match schema {
        Value::Object(map) => {
            for (name, spec) in map {
                if let Some(f) = field_from(owner, name, spec) {
                    out.push(f);
                }
            }
        }
        Value::Array(list) => {
            for spec in list {
                let Some(name) = spec.get("key").and_then(|v| v.as_str()) else {
                    continue;
                };
                if let Some(f) = field_from(owner, name, spec) {
                    out.push(f);
                }
            }
        }
        _ => {}
    }
}

fn field_from(owner: &str, name: &str, spec: &Value) -> Option<Field> {
    let obj = spec.as_object()?;
    // 键一律是 `<owner>.<name>`：插件写 "greeting" 和写 "miy.hello.greeting" 等价，
    // 但插件不可能把键指到别人的命名空间去。
    let prefix = format!("{owner}.");
    let key = if name.starts_with(&prefix) {
        name.to_string()
    } else {
        format!("{owner}.{name}")
    };
    let get_str = |k: &str| -> Option<String> {
        obj.get(k).and_then(|v| v.as_str()).map(|s| s.to_string())
    };
    Some(Field {
        key,
        owner: owner.to_string(),
        title: get_str("title").unwrap_or_else(|| name.to_string()),
        kind: get_str("type").unwrap_or_else(|| "string".to_string()),
        default: obj.get("default").cloned().unwrap_or(Value::Null),
        choices: obj
            .get("enum")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        min: obj.get("min").and_then(|v| v.as_f64()),
        max: obj.get("max").and_then(|v| v.as_f64()),
        control: get_str("control").unwrap_or_default(),
        step: obj.get("step").and_then(|v| v.as_f64()),
        group: get_str("group").unwrap_or_else(|| "通用".to_string()),
        order: obj.get("order").and_then(|v| v.as_i64()).unwrap_or(0),
        description: get_str("description").unwrap_or_default(),
    })
}

// ---- 读写 ------------------------------------------------------------------

fn effective_of(st: &State, f: &Field) -> Value {
    match st.doc.pointer("/values").and_then(|v| v.get(&f.key)) {
        Some(v) if !v.is_null() => v.clone(),
        _ => f.default.clone(),
    }
}

/// 给 UI 的快照：所有字段的 schema + 按其生效值解析后的 values。
pub fn snapshot() -> Value {
    with_state(|st| {
        let mut schema = Vec::with_capacity(st.fields.len());
        let mut values = Map::new();
        for f in &st.fields {
            schema.push(json!({
                "key": f.key,
                "owner": f.owner,
                "title": f.title,
                "type": f.kind,
                "default": f.default,
                "enum": f.choices,
                "min": f.min,
                "max": f.max,
                "control": f.control,
                "step": f.step,
                "group": f.group,
                "order": f.order,
                "description": f.description,
            }));
            values.insert(f.key.clone(), effective_of(st, f));
        }
        json!({ "schema": schema, "values": values })
    })
    .unwrap_or_else(|| json!({ "schema": [], "values": {} }))
}

/// 重新从盘上读一遍文档层（schema 层不动）。
///
/// 存在的理由：`settings.json` 是纯文本，用户和外部工具都可能直接改它；
/// 自检也靠它验证"未知字段原样保留"。热重载不重载插件，只重读这个文件。
pub fn reload() {
    let doc = load_doc();
    let _ = with_state(|st| {
        st.doc = doc;
    });
}

/// 按字符串读一个生效值，宿主内部用（比如启动时套用日志级别）。
pub fn effective_str(key: &str) -> Option<String> {
    with_state(|st| {
        let f = st.fields.iter().find(|f| f.key == key)?;
        match effective_of(st, f) {
            Value::String(s) => Some(s),
            other => Some(other.to_string()),
        }
    })
    .flatten()
}

/// 校验 + 落盘 + 广播 `settings.changed`。返回存下来的值。
pub fn set(key: &str, raw: Value) -> Result<Value, String> {
    let outcome = with_state(|st| {
        let field = st
            .fields
            .iter()
            .find(|f| f.key == key)
            .cloned()
            .ok_or_else(|| format!("未知设置项: {key}"))?;
        let value = coerce(&field, raw)?;

        // 只动 /values，其余字段原样留着。
        let root = st.doc.as_object_mut().ok_or("settings.json 根不是对象")?;
        let vals = root
            .entry("values")
            .or_insert_with(|| Value::Object(Map::new()));
        let vals = vals.as_object_mut().ok_or("settings.json 的 values 不是对象")?;
        vals.insert(key.to_string(), value.clone());
        save(&st.doc)?;
        Ok::<Value, String>(value)
    })
    .ok_or_else(|| "设置系统未初始化".to_string())??;

    apply_side_effects(key, &outcome);
    let payload = json!({ "key": key, "value": outcome }).to_string();
    bus::get().emit("settings.changed", payload.as_bytes());
    Ok(outcome)
}

/// 恢复默认：删掉覆盖值，让 default 层重新生效。文件里其它键不受影响。
pub fn reset(key: &str) -> Result<Value, String> {
    let restored = with_state(|st| {
        let field = st
            .fields
            .iter()
            .find(|f| f.key == key)
            .cloned()
            .ok_or_else(|| format!("未知设置项: {key}"))?;
        if let Some(vals) = st.doc.pointer_mut("/values").and_then(|v| v.as_object_mut()) {
            vals.remove(key);
        }
        save(&st.doc)?;
        Ok::<Value, String>(field.default)
    })
    .ok_or_else(|| "设置系统未初始化".to_string())??;

    apply_side_effects(key, &restored);
    let payload = json!({ "key": key, "value": restored }).to_string();
    bus::get().emit("settings.changed", payload.as_bytes());
    Ok(restored)
}

/// 少数设置项在宿主里有真实副作用，就在这里接。
fn apply_side_effects(key: &str, value: &Value) {
    if key == "core.log.level" {
        if let Some(s) = value.as_str() {
            crate::log::set_min_level(s);
        }
    }
}

fn coerce(f: &Field, raw: Value) -> Result<Value, String> {
    let v = match f.kind.as_str() {
        "bool" => match &raw {
            Value::Bool(b) => Value::Bool(*b),
            Value::String(s) => Value::Bool(matches!(
                s.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )),
            Value::Number(n) => Value::Bool(n.as_f64().unwrap_or(0.0) != 0.0),
            _ => return Err(format!("「{}」需要布尔值", f.title)),
        },
        "int" => {
            let n = as_f64(&raw).ok_or_else(|| format!("「{}」需要整数", f.title))?;
            Value::from(n.round() as i64)
        }
        "float" | "number" => {
            let n = as_f64(&raw).ok_or_else(|| format!("「{}」需要数字", f.title))?;
            json!(n)
        }
        "enum" => {
            if !f.choices.is_empty() && !f.choices.iter().any(|c| *c == raw) {
                return Err(format!("「{}」只能取 {}", f.title, choices_text(f)));
            }
            raw
        }
        _ => match raw {
            Value::String(s) => Value::String(s),
            Value::Null => Value::String(String::new()),
            other => Value::String(other.to_string()),
        },
    };

    if let Some(n) = v.as_f64() {
        if let Some(min) = f.min {
            if n < min {
                return Err(format!("「{}」不能小于 {min}", f.title));
            }
        }
        if let Some(max) = f.max {
            if n > max {
                return Err(format!("「{}」不能大于 {max}", f.title));
            }
        }
    }
    Ok(v)
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn choices_text(f: &Field) -> String {
    f.choices
        .iter()
        .map(|c| match c {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

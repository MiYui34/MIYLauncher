//! 崩溃分析：把 crash-report / latest.log 里的技术堆栈翻成「人话 + 怎么办」。
//!
//! 规则表按优先级从上到下匹配，第一条命中的胜出 —— 越具体的规则越靠前，
//! 兜底的退出码规则放最后。规则只做「文本匹配 + 少量解析」，不联网、不猜。
//!
//! 加规则的方式：写一个 `fn(&Facts, &str) -> Option<(String, String)>`，
//! 返回 `(结论, 建议)`，然后插进 `RULES` 里合适的位置。

/// 一次诊断的结果。
pub struct Diagnosis {
    /// 规则名，写进日志方便排查「为什么给了这条建议」。
    pub rule: &'static str,
    /// 一句话结论，直接显示给用户。
    pub title: String,
    /// 怎么办。
    pub advice: String,
}

/// 诊断需要的全部输入。
pub struct Facts<'a> {
    /// crash-report 全文（可能为空）。
    pub crash: &'a str,
    /// latest.log 的尾巴（可能为空）。
    pub log: &'a str,
    /// 进程退出码。
    pub code: i32,
    /// 实际用的 Java 大版本。
    pub java_major: Option<u32>,
    /// 版本 json 要求的 Java 大版本。
    pub required_java: Option<u32>,
}

type Rule = fn(&Facts, &str) -> Option<(String, String)>;

const RULES: &[(&str, Rule)] = &[
    ("java_old", rule_java_old),
    ("java_new", rule_java_new),
    ("oom", rule_oom),
    ("dup_mods", rule_dup_mods),
    ("mod_conflict", rule_mod_conflict),
    ("missing_dep", rule_missing_dep),
    ("mixin", rule_mixin),
    ("no_mc_class", rule_no_mc_class),
    ("assets", rule_assets),
    ("config", rule_config),
    ("glfw", rule_glfw),
    ("access", rule_access),
    ("access_violation", rule_access_violation),
];

/// 跑一遍规则表。没命中返回 `None`，调用方保留原来的兜底文案。
pub fn diagnose(f: &Facts) -> Option<Diagnosis> {
    let text = format!("{}\n{}", f.crash, f.log);
    for (name, rule) in RULES {
        if let Some((title, advice)) = rule(f, &text) {
            return Some(Diagnosis { rule: name, title, advice });
        }
    }
    None
}

/// 从 `class file version 65.0` / `Unsupported major.minor version 65.0` 里
/// 反推需要的 Java 大版本（65 → 21）。Java 1.1 是 45，所以减 44。
fn class_file_java(text: &str) -> Option<u32> {
    for key in ["class file version ", "Unsupported major.minor version "] {
        if let Some(i) = text.find(key) {
            let rest = &text[i + key.len()..];
            let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(major) = num.parse::<u32>() {
                if major >= 45 {
                    return Some(major - 44);
                }
            }
        }
    }
    None
}

/// 从堆栈里抠出 `mods/xxx.jar` 的文件名（去重、保持出现顺序）。
///
/// 全程按字节扫：崩溃报告里混着中文（`// 哎呀`），按字符下标切片会切在
/// 多字节字符中间直接 panic —— 分析崩溃的代码自己崩掉就太难看了。
fn mod_names(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    while i + 5 <= b.len() {
        let is_mods = b[i] == b'm'
            && b[i + 1] == b'o'
            && b[i + 2] == b'd'
            && b[i + 3] == b's'
            && (b[i + 4] == b'/' || b[i + 4] == b'\\');
        if !is_mods {
            i += 1;
            continue;
        }
        let start = i + 5;
        let mut end = start;
        while end < b.len() {
            let c = b[end];
            if c == b'.' && b[end..].starts_with(b".jar") {
                end += 4;
                break;
            }
            if c == b'/'
                || c == b'\\'
                || c == b'\n'
                || c == b'\r'
                || c == b'\t'
                || c == b'"'
                || c == b' '
            {
                break;
            }
            end += 1;
        }
        if end > start && b[start..end].ends_with(b".jar") {
            let name = String::from_utf8_lossy(&b[start..end]).into_owned();
            if !out.contains(&name) {
                out.push(name);
            }
        }
        i = end.max(i + 5);
    }
    out
}

fn rule_java_old(_f: &Facts, text: &str) -> Option<(String, String)> {
    if !(text.contains("UnsupportedClassVersionError")
        || text.contains("Unsupported major.minor version"))
    {
        return None;
    }
    let need = class_file_java(text);
    let title = match need {
        Some(n) => format!("Java 版本太低：这个版本需要 Java {n}"),
        None => "Java 版本太低".to_string(),
    };
    let advice = match need {
        Some(n) => format!(
            "在「设置 → Java」里换成 Java {n} 或更高版本；启动器也会在启动前自动下载需要的 Java。"
        ),
        None => "在「设置 → Java」里换一个更新的 Java。".to_string(),
    };
    Some((title, advice))
}

fn rule_java_new(f: &Facts, text: &str) -> Option<(String, String)> {
    let (have, need) = (f.java_major?, f.required_java?);
    if have <= need {
        return None;
    }
    let looks_like = text.contains("NoSuchMethodError")
        || text.contains("IncompatibleClassChangeError")
        || text.contains("IllegalAccessError")
        || text.contains("Unsupported class file major version");
    if !looks_like {
        return None;
    }
    Some((
        format!("Java 版本太高：这个版本要 Java {need}，当前是 Java {have}"),
        format!("在「设置 → Java」里指定 Java {need}，或者让启动器自动挑一个匹配的。"),
    ))
}

fn rule_oom(_f: &Facts, text: &str) -> Option<(String, String)> {
    let hit = text.contains("OutOfMemoryError")
        || text.contains("Java heap space")
        || text.contains("GC overhead limit exceeded")
        || text.contains("Metaspace");
    if !hit {
        return None;
    }
    Some((
        "内存不够：游戏把分配的内存用光了".to_string(),
        "在「设置 → 内存」里调大上限（整合包一般 4–6 GB），或者少装几个模组。".to_string(),
    ))
}

fn rule_dup_mods(_f: &Facts, text: &str) -> Option<(String, String)> {
    if !text.contains("DuplicateModsFoundException") {
        return None;
    }
    Some((
        "同一个模组装了多份".to_string(),
        "打开 mods 文件夹，把重复的那个 jar 删掉（文件名通常只差版本号）。".to_string(),
    ))
}

fn rule_mod_conflict(_f: &Facts, text: &str) -> Option<(String, String)> {
    let names = mod_names(text);
    if names.len() < 2 {
        return None;
    }
    let a = &names[0];
    let b = &names[1];
    Some((
        format!("模组冲突：{a} 和 {b} 改了同一处"),
        "先把这两个里的一个移出 mods 文件夹，能进游戏后再逐个加回来，加到哪个崩就是哪个。"
            .to_string(),
    ))
}

fn rule_missing_dep(_f: &Facts, text: &str) -> Option<(String, String)> {
    let hit = text.contains("Missing or unsupported mandatory dependencies")
        || text.contains("ModResolutionException")
        || (text.contains("requires") && text.contains("which is missing"));
    if !hit {
        return None;
    }
    Some((
        "模组缺前置".to_string(),
        "按崩溃报告里点名的模组去补前置（常见的是 Fabric API、Architectury、Cloth Config）。"
            .to_string(),
    ))
}

fn rule_mixin(_f: &Facts, text: &str) -> Option<(String, String)> {
    let hit = text.contains("MixinBootstrap")
        || text.contains("InvalidMixinException")
        || text.contains("Mixin apply failed")
        || text.contains("org.spongepowered.asm.mixin.transformer");
    if !hit {
        return None;
    }
    Some((
        "Mixin 注入失败：模组和加载器版本对不上".to_string(),
        "把加载器升到模组要求的版本；刚装过模组的话，先把它移出去试试。".to_string(),
    ))
}

fn rule_no_mc_class(_f: &Facts, text: &str) -> Option<(String, String)> {
    let hit = text.contains("NoClassDefFoundError: net/minecraft")
        || text.contains("ClassNotFoundException: net.minecraft")
        || text.contains("NoClassDefFoundError: net.minecraft");
    if !hit {
        return None;
    }
    Some((
        "客户端和加载器对不上".to_string(),
        "重新安装这个版本：启动器会把客户端和加载器重新合并成一个版本。".to_string(),
    ))
}

fn rule_assets(_f: &Facts, text: &str) -> Option<(String, String)> {
    let hit = text.contains("NoSuchFileException")
        && (text.contains("assets/objects") || text.contains("assets\\objects"));
    if !hit {
        return None;
    }
    Some((
        "资源文件缺失".to_string(),
        "点「修复」补齐资源，或者重新安装这个版本。".to_string(),
    ))
}

fn rule_config(_f: &Facts, text: &str) -> Option<(String, String)> {
    let hit = (text.contains("JsonParseException")
        || text.contains("JsonSyntaxException")
        || text.contains("MalformedJsonException"))
        && text.contains("config");
    if !hit {
        return None;
    }
    Some((
        "配置文件损坏".to_string(),
        "删掉版本目录下的 config 文件夹（或报告里点名的那个 .json），游戏会重新生成。"
            .to_string(),
    ))
}

fn rule_glfw(_f: &Facts, text: &str) -> Option<(String, String)> {
    let hit = text.contains("GLFW error")
        || text.contains("Failed to create window")
        || text.contains("PixelFormat")
        || text.contains("WGL")
        || (text.contains("OpenGL") && text.contains("1282"));
    if !hit {
        return None;
    }
    Some((
        "显卡驱动 / OpenGL 初始化失败".to_string(),
        "更新显卡驱动；笔记本双显卡的话，把 java.exe 设成用独显运行。".to_string(),
    ))
}

fn rule_access(_f: &Facts, text: &str) -> Option<(String, String)> {
    let hit = text.contains("AccessDeniedException")
        || text.contains("拒绝访问")
        || text.contains("The process cannot access the file");
    if !hit {
        return None;
    }
    Some((
        "文件被占用或没有权限".to_string(),
        "关掉杀毒软件的实时防护再试，或者把启动器目录加进白名单。".to_string(),
    ))
}

fn rule_access_violation(f: &Facts, _text: &str) -> Option<(String, String)> {
    // 0xC0000005 = STATUS_ACCESS_VIOLATION，Windows 上最常见的是显卡驱动或原生库。
    if f.code != -1073741819 && f.code != 0xC0000005u32 as i32 {
        return None;
    }
    Some((
        "游戏进程被系统强制结束（访问冲突）".to_string(),
        "多半是显卡驱动或模组里的原生库；先更新驱动，再逐个排查最近装的模组。".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(crash: &'a str, log: &'a str) -> Facts<'a> {
        Facts { crash, log, code: 1, java_major: None, required_java: None }
    }

    #[test]
    fn java_too_old_reads_class_version() {
        let f = facts(
            "java.lang.UnsupportedClassVersionError: net/minecraft/client/main/Main has been compiled by a more recent version of the Java Runtime (class file version 65.0)",
            "",
        );
        let d = diagnose(&f).expect("应该命中");
        assert_eq!(d.rule, "java_old");
        assert!(d.title.contains("Java 21"), "{}", d.title);
    }

    #[test]
    fn oom_wins_over_generic() {
        let f = facts("java.lang.OutOfMemoryError: Java heap space", "");
        let d = diagnose(&f).expect("应该命中");
        assert_eq!(d.rule, "oom");
    }

    #[test]
    fn mod_conflict_names_two_jars() {
        let f = facts(
            "at mods/sodium-0.5.8.jar\nat mods/embeddium-0.3.31.jar\n",
            "",
        );
        let d = diagnose(&f).expect("应该命中");
        assert_eq!(d.rule, "mod_conflict");
        assert!(d.title.contains("sodium-0.5.8.jar"), "{}", d.title);
        assert!(d.title.contains("embeddium-0.3.31.jar"), "{}", d.title);
    }

    #[test]
    fn single_mod_is_not_a_conflict() {
        let f = facts("at mods/sodium-0.5.8.jar\n", "");
        assert!(diagnose(&f).is_none());
    }

    #[test]
    fn assets_rule_needs_both_parts() {
        let f = facts("java.nio.file.NoSuchFileException: assets/objects/6a/abc", "");
        assert_eq!(diagnose(&f).unwrap().rule, "assets");
        let f2 = facts("java.nio.file.NoSuchFileException: mods/x.jar", "");
        assert!(diagnose(&f2).is_none());
    }

    #[test]
    fn access_violation_by_exit_code() {
        let f = Facts { crash: "", log: "", code: -1073741819, java_major: None, required_java: None };
        assert_eq!(diagnose(&f).unwrap().rule, "access_violation");
    }

    #[test]
    fn java_too_new_needs_both_versions() {
        let mut f = facts("java.lang.NoSuchMethodError: foo", "");
        f.java_major = Some(25);
        f.required_java = Some(21);
        assert_eq!(diagnose(&f).unwrap().rule, "java_new");
        f.java_major = Some(21);
        assert!(diagnose(&f).is_none());
    }
}

//! 编译进 exe 的「内置插件」资源。
//!
//! 目标只有一个：**下载一个 MIYLauncher.exe 就能启动游戏**。核心插件
//! `miy.mc`（版本清单、下载、启动）连同界面文件一起静态链接进来，用户
//! 不必再把 `plugins\` 目录跟 exe 打包在一起。
//!
//! 这里的 manifest 与 `plugins/miy-mc/manifest.json` 是**同一份文件**，
//! 编译期 `include_str!` 进来，改一边就是改两边，不会漂移。界面文件同理。
//!
//! 第三方插件仍然走磁盘（[`crate::bus::load_all`]），这条路没变。

/// 核心插件 manifest（id / version / 设置项 / 界面入口全在里面）。
pub const MC_MANIFEST: &str = include_str!("../../../plugins/miy-mc/manifest.json");

/// 内置插件贡献的页面正文。
///
/// `miy.app.page` 先查这里、再查磁盘：外置插件照旧读
/// `plugins/<id>/ui/<entry>`，内置插件命中的是编译进 exe 的同一份 HTML。
pub fn page(plugin: &str, entry: &str) -> Option<&'static str> {
    match (plugin, entry) {
        ("miy.mc", "home.html") => Some(crate::assets::home()),
        ("miy.mc", "versions.html") => Some(crate::assets::versions()),
        ("miy.mc", "games.html") => Some(crate::assets::games()),
        ("miy.mc", "mods.html") => Some(crate::assets::mods()),
        ("miy.mc", "detail.html") => Some(crate::assets::detail()),
        ("miy.mc", "modlist.html") => Some(crate::assets::modlist()),
        _ => None,
    }
}

# MIYLauncher

一个 Minecraft Java 版启动器：**Rust + WebView2 + HTML**，编译出来是一个 1.3 MB 的单 exe，
「一切皆插件」——宿主只负责窗口、进程、网络、文件与插件调度，游戏相关的逻辑全在插件里。

```
MIYLauncher.exe          宿主（1.3 MB，静态 CRT，无需运行库）
plugins\miy.mc\          版本 / 下载 / 启动 / 登录 / 模组
plugins\miy.hello\       插件示例
```

## 特性

- **只做该做的事**：微软账号正版登录、离线身份、版本安装与启动、模组、整合包导入、配置。没有皮肤站、没有整合包市场、没有社区。
- **单文件、免安装**：静态链接 CRT，不依赖 VC++ 运行库；数据目录默认就在 exe 旁边，整个文件夹拷走就能用。
- **插件化**：插件是 `cdylib`（`.dll`）+ 一份 `manifest.json` + 自己的 `ui/*.html`，可以加导航页、首页卡片、设置项、界面页面。
- **崩溃隔离**：插件 panic / 访问违例不会带走宿主，出问题的插件会被熔断并记一条日志。
- **界面**：Material 3 浅色，无边框窗口 + 自绘窗口按钮，页面按插件分片加载。

## 架构速览

| 层 | 位置 | 说明 |
|---|---|---|
| ABI | `crates/miy-abi`、`include/miy_plugin.h` | `miy_plugin_entry_v1()` 返回 `MiyPluginV1`；宿主给 `MiyHostV1`（log / alloc / free / call / task / emit） |
| 宿主 | `crates/miy-host` | 窗口、WebView2、IPC、插件总线、设置、网络、日志 |
| 插件 | `plugins/*` | 每个插件一个 crate，`crate-type = ["cdylib"]` |

三条不变量（`crates/miy-host/src/bus.rs` 文件头）：同一个插件的 handler 永不并发；每次跨边界调用都有 deadline；
handler 里再回调插件一律返回 `MIY_ERR_REENTRANT`（handler 内可以调宿主服务）。

## 构建

需要 Rust（MSVC 工具链）、Windows SDK（`rc.exe` 编译图标）与 WebView2 运行时（Win10/11 自带）。

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build.ps1          # 构建 + 打包到 dist\
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build.ps1 -Verify  # 构建 + 自检 + 窗口验收
```

产物在 `dist\`：`MIYLauncher.exe`、`plugins\<插件id>\`（dll + manifest + ui）。

## 插件开发

`docs/plugin-dev.md` 是完整的第三方插件指南（ABI、清单字段、服务清单、页面桥、权限与常见坑）。

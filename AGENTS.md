# WinKeepAwake — Agent 规则

## 定位
Windows 原生防息屏/防锁定工具：eframe/egui GUI + 系统托盘后台常驻，**双工作模式**——
防息屏（注入 + 完整电源声明，屏幕常亮）/ 息屏防睡（到点关屏但系统不休眠：无注入，
`ES_SYSTEM|ES_AWAYMODE` 声明 + powercfg 动态改写关屏超时），键盘层(F13)默认开启，
支持定时自动停止与托盘一键重启（`--screen-off-guard <mins>` 启动参数交接状态）。

## 怎么跑
- 开发：`cargo run`（debug 带控制台）
- 发布：`cargo build --release` → `target/release/WinKeepAwake.exe`（单文件、无控制台、约 5MB）
- 门禁：`cargo test`（38 项）、`cargo clippy --all-targets` 必须零告警；
  真机 powercfg 往返用 `cargo test -- --ignored` 手动跑（会临时改写 VIDEOIDLE 并还原）

## 技术栈
- Rust `edition 2021`，仅目标 Windows 10/11。
- `eframe/egui 0.36`（App 已拆 `ui()`+`logic()`；`on_exit` 签名带 `_gl: Option<&eframe::glow::Context>`）。
- `tray-icon 0.26` + `raw-window-handle 0.6`；`windows 0.62`（Win32 绑定）。
- 图标/中文字体均运行时生成或从系统加载，**不引入外部资源文件**（独立 exe 是硬约束）。

## 目录与约定
- `main.rs` GUI/托盘/窗口显隐/退出路径；`worker.rs` 后台保活线程（命令通道 + 快照 + 模式切换 + 电源漂移校验 + 分级日志）；
  `jitter.rs` 纯 Win32 封装（`keep_awake_active` 常亮档 / `keep_awake_system_only` 息屏防睡档）；
  `power.rs` powercfg 封装（VIDEOIDLE/VIDEOCONLOCK 读写、备份恢复、`display_off_effective` 漂移校验）；
  `icon.rs` 程序化图标（SDF + 2× 超采样，双设计：咖啡=防息屏 / 月牙=息屏防睡），
  `build.rs` 经 `#[path]` 复用它生成多尺寸 .ico 并用 winresource 嵌入 exe（需 Windows SDK rc.exe）。
- 保活逻辑必须跑在独立 worker 线程，不得依赖 GUI 重绘节奏（窗口隐藏时仍需持续）。
- 窗口显隐走原生 `ShowWindow`，不要用 `ViewportCommand::Visible`（有隐藏后无法再显示的已知问题）。
- **安全不变量（不许破坏）**：允许关屏（关屏超时 ≠ 0）⟺ 工具正以息屏防睡运行。任何暂停/切换/退出路径
  必须恢复电源计划；`std::process::exit` 不走 eframe `on_exit`，托盘退出/重启必须显式调 `power::end_display_off()`。
- 单元测试绝不真改电源计划：涉及电源路径的 worker 测试必须注入 `TestHooks`（power_begin/power_end/power_effective）。
- 改 `windows`/`egui` API 前先核对本地 registry 源码版本签名——这两个 crate 频繁 breaking change。

## 当前状态与下一步
- 阶段一/二（三层防御 + GUI/托盘/定时停止）与阶段三（2.2.0：息屏防睡模式、电源计划动态管理、GPO 漂移校验、
  托盘重启）均已实现并通过 test/clippy/E2E（真机验证过 powercfg 写入、ES 声明、模式切换恢复）。
- 2.3.0（2026-11-18）：GUI 翻新（分段模式选择器/小节布局/进度条/警示横幅/分级彩色日志/动态标题/状态徽标）、
  exe 嵌入图标与 VERSIONINFO、托盘图标随模式切换、`--dump-icons` 调试出口。
- 2.3.1（2026-11-19）：按用户反馈二轮打磨——修复托盘左键弹菜单（tray-icon 0.26 `menu_on_left_click` 默认 true，
  已显式关闭）；GUI 去「AI 味」：扁平化（去卡片/胶囊徽标）、真实图标进头部、行式设置、全等宽日志；
  模式设置区固定高度修切换抖动；日志区占满剩余高度（默认窗口 460×840）。
  注意坑：CLI 交接模式下 GUI 草稿要在用户操作前持续跟随 worker 快照（`ui_mode_touched`），
  一次性首帧同步会因 worker 秒级轮询排队而读到旧值；GUI 视觉验证用 PrintWindow 抓窗
  （CopyFromScreen 会被前台窗口遮挡，SetForegroundWindow 受前台权限限制常静默失败）；
  egui 0.36 的 `Image::new` 只收 source（尺寸用 `fit_to_exact_size`），Slider 无宽度方法（用 `ui.add_sized`）。
- **本机已知事实**（写代码/排障时必须记住）：开发机 ThinkBook 14 G8 IAL 三次夜间挂死均发生在
  「屏幕已关 + PowerToys Awake 持 SYSTEM 声明」环境下——系统级保活声明挡不住该机关屏路径挂死（嫌疑：核显驱动
  igdkmdn64 关屏/runtime D3，12× 0x141）。所以息屏防睡在 UI/README 里标注为实验性，过夜推荐防息屏模式。
- 本机电源计划受域 GPO 周期重置（手工值数小时内会被改回默认 600/300），故 worker 有 5 分钟漂移校验。
- 单元测试只断言确定性逻辑；「空闲是否真被归零」「关屏是否发生」属系统时序，用集成脚本验证，勿在 `cargo test` 里断言时序。
- git 仓库已存在（早期文档说法过时）；提交需用户明确要求。

# WinKeepAwake — Agent 规则

## 定位
Windows 原生防息屏/防锁定工具：eframe/egui GUI + 系统托盘后台常驻，
「鼠标零位移注入 + SetThreadExecutionState」双层主力，键盘层(F13)可选默认关闭，支持定时自动停止。

## 怎么跑
- 开发：`cargo run`（debug 带控制台）
- 发布：`cargo build --release` → `target/release/WinKeepAwake.exe`（单文件、无控制台、约 5MB）
- 门禁：`cargo test`（21 项）、`cargo clippy --all-targets` 必须零告警

## 技术栈
- Rust `edition 2021`，仅目标 Windows 10/11。
- `eframe/egui 0.36`（App 已拆 `ui()`+`logic()`；样式用 `global_style_mut`/`set_visuals`；字体用 `add_font(FontInsert)`，路径在 `egui::epaint::text`）。
- `tray-icon 0.26` + `raw-window-handle 0.6`；`windows 0.62`（Win32 绑定）。
- 图标/中文字体均运行时生成或从系统加载，**不引入外部资源文件**（独立 exe 是硬约束）。

## 目录与约定
- `main.rs` GUI/托盘/窗口显隐；`worker.rs` 后台保活线程（命令通道 + 快照）；`jitter.rs` 纯 Win32 封装；`icon.rs` 程序化图标；`font.rs` 中文字形回退。
- 保活逻辑必须跑在独立 worker 线程，不得依赖 GUI 重绘节奏（窗口隐藏时仍需持续）。
- 窗口显隐走原生 `ShowWindow`，不要用 `ViewportCommand::Visible`（有隐藏后无法再显示的已知问题）。
- 改 `windows`/`egui` API 前先核对本地 registry 源码版本签名——这两个 crate 频繁 breaking change。

## 当前状态与下一步
- 阶段一（控制台三层防御）与阶段二（GUI/托盘/定时停止/无感注入/可关闭键盘层）均已实现并通过 build/test/clippy/E2E。
- 单元测试只断言确定性逻辑；「空闲是否真被归零」属系统时序，用集成脚本（powercfg + GetLastInputInfo 观测）验证，勿在 `cargo test` 里断言时序。
- 尚无 git 仓库；如需版本管理需另行初始化。

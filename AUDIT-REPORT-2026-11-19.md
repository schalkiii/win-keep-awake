# 审计报告 · WinKeepAwake（2026-11-19）

> 由 project-audit 技能产出。本文档只登记发现与建议，不含改动。
> 执行阶段按「批次 A → B → C」串行（文件有重叠，禁止并行），并拼接 software-improvement 的改动纪律。

## 一、现状描述

- **规模**：7 个 Rust 源文件共 3354 行——`main.rs` 1060（GUI/托盘/窗口管理）、`worker.rs` 1185（后台保活线程 + 测试，其中测试约 520 行）、`icon.rs` 352（SDF 程序化图标）、`jitter.rs` 340（Win32 封装）、`power.rs` 236（powercfg 封装）、`font.rs` 87（CJK 字体回退）、`build.rs` 94（构建期图标嵌入）。
- **结构**：三层——`jitter`/`power`（纯 Win32 封装）→ `worker`（独立线程，mpsc 命令通道 + `Arc<Mutex<Snapshot>>` 快照回发）→ `main`（egui 渲染 + 托盘）。分层清晰，UI 与保活逻辑解耦，方向正确。
- **质量水平**：README 132 行（架构/边界/构建/使用齐全），AGENTS.md 51 行（含本机故障背景与坑位记录）；38 项单元测试 + 1 项真机 ignored 测试全绿；clippy `-D warnings` 零告警；CI（windows-latest：build/test/clippy）绿。两天内从 2.0 迭代到 2.3.3（7 个版本），文档更新存在滞后残留。

## 二、七维度结论

| # | 维度 | 结论 |
|---|------|------|
| 1 | 文档一致性 | **发现 6 处过时**：H1、M1、M2、M3、L3、L4（详见下表） |
| 2 | 文档缺漏 | 未发现关键缺漏——架构、安全边界、构建、CLI 参数（`--dump-icons`/`--screen-off-guard`）均有文档 |
| 3 | 架构优化点 | M4：快照克隆模式存在维护陷阱 |
| 4 | 代码写法 | M5、M6、L1、L2、L7 |
| 5 | 测试覆盖率 | H3（键盘层契约零覆盖）、M8（电源备份语义零自动化覆盖） |
| 6 | 测试严谨性 | H2（一个测试漏真实系统输入）、L6（测试样板重复） |
| 7 | 工程规范 | M7（CI 缺 fmt 检查）、L5（本地孤儿截图） |

## 三、问题清单

| 编号 | 位置 | 问题 | 严重程度 | 影响范围 |
|------|------|------|----------|----------|
| H1 | `src/jitter.rs:111,120` | `perform_keyboard_injection` 文档注释写「可选，默认关闭」「仅在用户显式开启时才会用到」——2.3.2 起键盘层已默认开启（`worker.rs` Config `keyboard_enabled: true`） | 高 | 安全相关默认值方向写反：后续维护者/二次开发会误判契约，可能据此做出错误取舍 |
| H2 | `src/worker.rs:948`（`throttle_allows_single_injection_per_window`） | `kb_inject: None` + 键盘层默认开启 → 该测试触发**真实 F13 SendInput**，突破「worker 测试全走钩子」的设计；且与 `jitter.rs` 真注入测试并行时无 `input_lock` 串行化，存在偶发竞态 | 高 | 测试隔离性破坏 + 潜在 flaky；违反项目自身「确定性单元测试」纪律 |
| H3 | `src/worker.rs` 测试区 | v2.3.3 明确的键盘层契约——「与鼠标同周期叠加；关闭键盘层不影响鼠标层」——**零测试覆盖**。`Command::KeyboardEnabled` 的行为效果（钩子是否被调、关掉后鼠标是否照注）没有任何用例锁住 | 高 | 该契约回退（如误改成互斥/替代关系）不会被测试捕获 |
| M1 | `README.md:60` | 功能特性仍列「实验性模式橙色警示横幅（长文收进悬停提示）」——2.3.2 已删除该横幅；现存的橙色横幅只用于「电源写入失败安全降级」 | 中 | 用户按 README 找不到承诺的 UI 元素 |
| M2 | `README.md:25` | 「UI 内也有同样的警告」不实——UI 现在只在电源写入失败时警示，过夜风险提示只在 README | 中 | 用户误以为 UI 会提醒过夜风险 |
| M3 | `AGENTS.md:48` | 「息屏防睡在 UI/README 里标注为实验性」——UI 部分过时（横幅已删）；且「当前状态」缺 2.3.2/2.3.3 两条记录 | 中 | Agent 规则文件与实际 UI 行为不符，误导后续会话 |
| M4 | `src/main.rs:151-167`（`snapshot_clone`） | 手工逐字段克隆 Snapshot——Snapshot 新增字段时忘更此函数，GUI 将**静默显示旧值**（编译器不报错） | 中 | 维护陷阱：每次给 Snapshot 加字段都是一次隐患 |
| M5 | `src/main.rs:462`（`controls`） | 每帧调 `app.snapshot_clone()`（含 120 条日志的 VecDeque 深拷贝）只为读一个 `status == Running` 布尔 | 中 | 每帧两份全量快照克隆（ui() 一份 + controls() 一份），纯浪费 |
| M6 | `src/main.rs:33` 与 `src/worker.rs:97` | 默认空闲阈值 50 双份定义（`DEFAULT_IDLE_SECS` 常量 vs `Snapshot::default` 硬编码）——改一处漏一处则 GUI 草稿与 worker 初值分叉 | 中 | 同类维护陷阱 |
| M7 | `.github/workflows/ci.yml` | CI 只有 build/test/clippy，无 `cargo fmt --check`——格式漂移只能靠本地自觉 | 中 | 公开仓库接受 PR 时格式无法把关 |
| M8 | `src/power.rs:124-137` | `begin_display_off` 的「已有备份不覆盖」幂等分支（安全不变量的核心：防止把自己的写入值误存为原始值）**无任何自动化覆盖**——真机 ignored 测试只测 VIDEOIDLE 数值往返，不测备份语义 | 中 | 此分支回归 = 退出时把临时值恢复成「工具写入值」而非用户原值，属静默的安全不变量破坏 |
| L1 | `src/main.rs:253-257` | `worker_keep_awake_release_via_cmd` 名不副实——函数并不直接释放声明，只发暂停命令 | 低 | 可读性 |
| L2 | `src/main.rs:906-908, 918-920` | 托盘「重启/退出」两处重复 `end_display_off + keep_awake_release + exit` 三连 | 低 | 重复代码 |
| L3 | `src/worker.rs:233` | 启动日志「保活已启动（鼠标零位移 + 电源管理声明）」未提 F13 键盘层（默认已开），与 2.3.2+ 实际行为不符 | 低 | 日志与实际防线不符 |
| L4 | `src/main.rs:1-7` | 顶部模块注释中 worker 职责清单缺「模式切换、电源计划启停与漂移校验」 | 低 | 文档完整性 |
| L5 | 仓库根 `gui_capture.png` | 10-12 旧版 GUI 截图，未被 git 跟踪（`.gitignore` 已含 `*.png`），README 无引用——纯本地残留 | 低 | 本地工作区整洁度；**删除需用户确认** |
| L6 | `src/worker.rs` 测试区 | 6 个测试手搓 `WorkerOpts{...}` 而不复用 `fast_opts`/`guard_opts` | 低 | 测试样板重复 |
| L7 | `src/main.rs`/`src/worker.rs` vs `src/power.rs` | 锁中毒处理不一致：power.rs 用 `unwrap_or_else(\|e\| e.into_inner())`，其余处 `unwrap()` | 低 | 一致性 |

## 四、改进建议（按批次）

### 批次 A · 文档与注释同步（对应 H1、M1、M2、M3、L3、L4）

1. **H1**：重写 `src/jitter.rs:111-120` 文档注释——去掉「默认关闭」表述，改为「默认开启，作为鼠标注入的叠加补充；与鼠标注入同一保活周期触发」；保留 F13 选型理由。
2. **M1**：`README.md:60` 改为「电源写入失败时的橙色警示横幅（安全降级提示 + 原因，长文收进悬停）」。
3. **M2**：`README.md:25` 删去「UI 内也有同样的警告」，改为指向 UI 的实际行为（仅电源写入失败时警示降级）。
4. **M3**：`AGENTS.md:48` 同步横幅移除事实；「当前状态」补 2.3.2/2.3.3 一行。
5. **L3**：`src/worker.rs:233` 启动日志改为「保活已启动（鼠标零位移 + F13 键盘层 + 电源管理声明）」。
6. **L4**：`src/main.rs` 顶部注释 worker 职责补全。

- **文件清单**：`src/jitter.rs`、`README.md`、`AGENTS.md`、`src/worker.rs`、`src/main.rs`
- **验证方式**：`cargo test` + `cargo clippy --all-targets` 全绿；`grep -n "默认关闭\|实验性模式橙色" src/*.rs README.md` 零命中；人工复读 diff 确认无过度改写。

### 批次 B · 代码质量（对应 M4、M5、M6、L1、L2）

1. **M4**：`Snapshot` 加 `#[derive(Clone)]`，`snapshot_clone()` 改为锁内 `.clone()`——消除逐字段清单，新增字段自动纳入。
2. **M5**：`controls()` 改为锁内直读 `status`（Copy 字段，零克隆）。
3. **M6**：`DEFAULT_IDLE_SECS` 移到 `worker.rs` 导出（`pub const`），`Snapshot::default` 与 `main.rs` 共用单一来源。
4. **L1**：`worker_keep_awake_release_via_cmd` 更名为 `pause_worker_via_cmd`，注释同步。
5. **L2**：托盘重启/退出的三连清理提取为 `fn tray_exit_cleanup()`。

- **文件清单**：`src/main.rs`、`src/worker.rs`
- **验证方式**：`cargo test` + `cargo clippy --all-targets` 零告警 + `cargo build --release`；真机启动窗口标题仍为「WinKeepAwake · 防息屏 · 运行中」（行为无回归）。

### 批次 C · 测试补强与 CI（对应 H2、H3、M8、M7）

1. **H2**：`throttle_allows_single_injection_per_window` 的 `kb_inject` 补 `Some(Box::new(|| true))` 钩子，恢复测试密闭性。
2. **H3**：新增两个 worker 测试锁住键盘层契约：
   - `keyboard_disabled_mouse_still_fires`：`KeyboardEnabled(false)` 后空闲注入仍计数、kb 钩子计数为 0；
   - `keyboard_enabled_fires_alongside_mouse`：kb 钩子计数与注入计数同步增长。
3. **M8**：`src/power.rs` 新增 `#[ignore]` 真机测试 `real_backup_not_overwritten_by_reapply`：begin(300) → begin(600) → end() 应回复**最初**原值（而非 300），锁住备份不覆盖语义。
4. **M7**：CI 增加 `cargo fmt --all -- --check` 步骤（dtolnay action 的 components 加 `rustfmt`）。

- **文件清单**：`src/worker.rs`、`src/power.rs`、`.github/workflows/ci.yml`
- **验证方式**：`cargo test` 全绿且新增用例真实通过（非编译通过即弃）；`cargo test -- --ignored` 手动跑备份语义测试（真机）；推送后 CI 绿。

## 五、优先级与并行性

- **执行顺序**：批次 A → B → C **串行**（三者均触碰 `src/worker.rs`，A/B 还都触碰 `src/main.rs`，文件重叠禁止并行）。
- **批次内**：逐点改动、逐点跑 `cargo test`，禁止批量改完统一验证。
- **不修项**：L5（本地文件删除，等用户确认）、L6（纯测试样板美观，改动收益低于噪音，记录不动）、L7（全仓锁中毒风格统一属大范围机械改动，风险大于收益，记录不动）。
- **版本**：三批次完成后 Cargo.toml 版本 bump 至 2.3.4 一并发布。

## 六、执行期新发现（已随批次 C 一并修复）

| 编号 | 位置 | 问题 | 处置 |
|------|------|------|------|
| X1 | `src/power.rs` 两个 `#[ignore]` 真机测试 | 默认并行跑时互相读写同一电源计划且共享静态 `BACKUP`——对方的写入会被误认成「原值」备份，**污染系统基线**（2026-11-19 实测复现：VIDEOIDLE AC 被残留为探针值 555） | 已修：仿 jitter.rs `input_lock` 加 `real_power_lock()` 全局锁强制串行；机器基线已手工恢复（VIDEOIDLE 600/300、VIDEOCONLOCK 0/0）并经 `cargo test -- --ignored` 双测试连跑验证无损 |

> 教训（已并入项目纪律）：真机副作用测试（电源计划、真实输入）之间必须显式串行化，
> 「各自能单独跑过」不代表「一起跑安全」——备份/恢复语义遇到并发就会把污染值漂白成合法原值。

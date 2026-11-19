//! WinKeepAwake —— 现代 GUI + 系统托盘版防屏幕超时锁定工具。
//!
//! 三层架构：
//! 1. `worker` 线程：独立于渲染循环的后台保活逻辑（空闲检测、注入、电源声明、定时自动停止）。
//! 2. 本文件的 `App`：eframe/egui 界面，只负责展示快照 + 收集配置并通过通道下发命令。
//! 3. 托盘图标：通过 `set_event_handler` 回调直接调用 Win32 `ShowWindow` 显隐窗口，
//!    不依赖 eframe 的 `ViewportCommand::Visible`（该 API 在部分版本存在"隐藏后无法再次显示"问题）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod font;
mod icon;
mod jitter;
mod power;
mod worker;

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{
    self, Align, Color32, CornerRadius, IconData, Margin, ProgressBar, RichText, ScrollArea,
    Stroke, TextStyle, Vec2, Visuals,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::icon::{build_icon_rgba, build_mode_icon_rgba, IconVariant};
use crate::jitter::{window_hide, window_is_visible, window_show, MouseMode};
use crate::worker::{Command, LogLevel, Mode, Snapshot, Status, DEFAULT_DISPLAY_OFF_MINS};

const DEFAULT_IDLE_SECS: u64 = 50;

const MENU_ID_SHOW_HIDE: &str = "show_hide";
const MENU_ID_TOGGLE_PAUSE: &str = "toggle_pause";
const MENU_ID_RESTART: &str = "restart";
const MENU_ID_QUIT: &str = "quit";

// ------------------------------------------------------------------ 调色板 ----

const ACCENT_COLOR: Color32 = Color32::from_rgb(56, 189, 248); // 亮青（防息屏/品牌）
const GUARD_COLOR: Color32 = Color32::from_rgb(129, 140, 248); // 靛蓝（息屏防睡）
const GREEN: Color32 = Color32::from_rgb(52, 211, 153);
const AMBER: Color32 = Color32::from_rgb(251, 191, 36);
const ORANGE: Color32 = Color32::from_rgb(251, 146, 60);
const RED: Color32 = Color32::from_rgb(248, 113, 113);
const TEXT_MAIN: Color32 = Color32::from_rgb(228, 233, 243);
const TEXT_DIM: Color32 = Color32::from_rgb(152, 160, 177);
const TEXT_FAINT: Color32 = Color32::from_rgb(112, 120, 138);
const PANEL_FILL: Color32 = Color32::from_rgb(17, 19, 26);
const CARD_FILL: Color32 = Color32::from_rgb(24, 26, 34);
const CARD_STROKE: Color32 = Color32::from_rgb(42, 45, 58);
const INSET_FILL: Color32 = Color32::from_rgb(28, 31, 41);

/// 颜色变暗（保持不透明度），用于徽标底色/描边。
fn tint(c: Color32, f: f32) -> Color32 {
    let m = |v: u8| ((v as f32 * f).round().clamp(0.0, 255.0)) as u8;
    Color32::from_rgb(m(c.r()), m(c.g()), m(c.b()))
}

/// 主窗口标题条内容 + 状态标签等 UI 元素需要的运行时状态，全部来自 worker 线程的只读快照。
struct MyApp {
    snapshot: Arc<Mutex<Snapshot>>,
    cmd_tx: Arc<Mutex<Sender<Command>>>,
    hwnd_raw: isize,
    _tray: Option<TrayIcon>,

    // GUI 侧编辑的配置草稿（初次显示时与 worker 默认值保持一致，之后由用户操作实时下发）
    ui_idle_secs: u64,
    ui_mouse_mode: MouseMode,
    ui_keyboard_enabled: bool,
    ui_auto_stop_enabled: bool,
    ui_auto_stop_mins: u64,
    ui_lock_on_stop: bool,
    ui_minimize_on_close: bool,
    ui_mode: Mode,
    ui_display_off_mins: u64,

    /// 托盘图标当前呈现的（模式, 是否运行），变化时才重建图标/更新提示，避免每帧抖动。
    tray_state: Option<(Mode, bool)>,
    /// 当前窗口标题缓存：标题里带模式与运行状态，仅在变化时下发 ViewportCommand::Title。
    last_title: String,
    /// 模式/息屏分钟是否已被用户操作过。未操作前每帧跟随 worker 快照——
    /// CLI 交接（`--screen-off-guard`）的模式命令在 worker 的秒级轮询里排队，
    /// 一次性首帧同步会赶在 worker 处理前读到旧值；持续跟随直到用户点击为止。
    ui_mode_touched: bool,
    /// 头部真实图标纹理（64px 渲染，随模式切换），启动时加载一次终身复用。
    icon_awake: egui::TextureHandle,
    icon_guard: egui::TextureHandle,
}

impl MyApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        snapshot: Arc<Mutex<Snapshot>>,
        cmd_tx: Arc<Mutex<Sender<Command>>>,
    ) -> Self {
        apply_theme(&cc.egui_ctx);
        // 中文字形回退：egui 自带字体不含 CJK，不注入会出现"豆腐块"
        font::install_cjk_font(&cc.egui_ctx);

        let hwnd_raw = extract_hwnd(cc);

        let tray = setup_tray(hwnd_raw, cmd_tx.clone(), snapshot.clone());

        // 头部直接展示真实渲染的图标（与托盘/exe 同源），替代几何色块拼贴。
        let icon_awake = cc.egui_ctx.load_texture(
            "header-icon-awake",
            egui::ColorImage::from_rgba_unmultiplied(
                [64, 64],
                &build_mode_icon_rgba(64, IconVariant::Awake),
            ),
            egui::TextureOptions::default(),
        );
        let icon_guard = cc.egui_ctx.load_texture(
            "header-icon-guard",
            egui::ColorImage::from_rgba_unmultiplied(
                [64, 64],
                &build_mode_icon_rgba(64, IconVariant::NightGuard),
            ),
            egui::TextureOptions::default(),
        );

        Self {
            snapshot,
            cmd_tx,
            hwnd_raw,
            _tray: tray,
            ui_idle_secs: DEFAULT_IDLE_SECS,
            ui_mouse_mode: MouseMode::ZeroMove,
            ui_keyboard_enabled: false,
            ui_auto_stop_enabled: false,
            ui_auto_stop_mins: 60,
            ui_lock_on_stop: false,
            ui_minimize_on_close: true,
            ui_mode: Mode::KeepScreenOn,
            ui_display_off_mins: DEFAULT_DISPLAY_OFF_MINS,
            tray_state: None,
            last_title: String::new(),
            ui_mode_touched: false,
            icon_awake,
            icon_guard,
        }
    }

    fn send_cmd(&self, cmd: Command) {
        let _ = self.cmd_tx.lock().unwrap().send(cmd);
    }

    fn snapshot_clone(&self) -> Snapshot {
        let s = self.snapshot.lock().unwrap();
        Snapshot {
            status: s.status,
            idle_threshold_secs: s.idle_threshold_secs,
            current_idle_ms: s.current_idle_ms,
            effective_mouse_mode: s.effective_mouse_mode,
            auto_stop_remaining_secs: s.auto_stop_remaining_secs,
            auto_stop_total_secs: s.auto_stop_total_secs,
            injections_done: s.injections_done,
            mode: s.mode,
            display_off_mins: s.display_off_mins,
            display_off_active: s.display_off_active,
            power_note: s.power_note.clone(),
            log: s.log.clone(),
        }
    }
}

impl eframe::App for MyApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 即使窗口隐藏到托盘，只要这里周期性请求重绘，logic 会被持续调用；
        // 但核心的保活逻辑已经不依赖这个方法（跑在独立 worker 线程），这里仅用于
        // 让"窗口重新可见时"UI 数据保持实时刷新，代价很低（每 500ms 一次）。
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // 1. 拦截窗口关闭请求：转换为"最小化到托盘"（若开启该选项），而不是真的退出进程。
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if close_requested {
            if self.ui_minimize_on_close {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                window_hide(self.hwnd_raw);
            } else {
                // 进程即将退出：直接恢复电源计划（worker 可能来不及排空通道命令）。
                crate::power::end_display_off();
                worker_keep_awake_release_via_cmd(&self.cmd_tx);
            }
        }

        let snap = self.snapshot_clone();

        // 模式/息屏分钟：用户未操作前持续跟随 worker（覆盖 CLI 交接的排队延迟），
        // 用户点击分段选择器/滑杆后改由 GUI 乐观更新，不再回写覆盖。
        if !self.ui_mode_touched {
            self.ui_mode = snap.mode;
            self.ui_display_off_mins = snap.display_off_mins;
        }

        // 动态窗口标题：任务栏/Alt-Tab 里一眼可辨「哪个模式、是否在跑」。
        let running = snap.status == Status::Running;
        let title = format!(
            "WinKeepAwake · {} · {}",
            mode_label(snap.mode),
            status_label(snap.status)
        );
        if self.last_title != title {
            self.last_title = title.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
        }

        // 托盘图标随模式切换（咖啡=防息屏 / 月牙=息屏防睡），提示文字带运行状态。
        if self.tray_state != Some((snap.mode, running)) {
            self.tray_state = Some((snap.mode, running));
            if let Some(tray) = &self._tray {
                let variant = match snap.mode {
                    Mode::KeepScreenOn => IconVariant::Awake,
                    Mode::ScreenOffGuard => IconVariant::NightGuard,
                };
                if let Ok(icon) = Icon::from_rgba(build_mode_icon_rgba(32, variant), 32, 32) {
                    let _ = tray.set_icon(Some(icon));
                }
                let _ = tray.set_tooltip(Some(format!(
                    "WinKeepAwake · {}（{}）",
                    mode_label(snap.mode),
                    status_label(snap.status)
                )));
            }
        }

        // 扁平布局：无卡片嵌套，所有内容统一缩进到面板边距，段落用留白+小节标题分组。
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(2.0);
            header(ui, &snap, &self.icon_awake, &self.icon_guard);
            ui.add_space(10.0);
            hero(ui, &snap);
            ui.add_space(10.0);
            controls(ui, self);
            log_section(ui, &snap);
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 任何 eframe 正常退出路径的兜底：绝不让「允许关屏」的电源计划改动在进程外残留。
        // （托盘菜单的退出/重启走 std::process::exit，不经过这里，它们各自显式调用。）
        crate::power::end_display_off();
    }
}

fn worker_keep_awake_release_via_cmd(cmd_tx: &Arc<Mutex<Sender<Command>>>) {
    // 关闭窗口且未开启"最小化到托盘"时：先释放电源声明，再退出进程（托盘菜单退出走的是
    // jitter::keep_awake_release 直接调用，这里复用同一语义，走通道通知 worker 停止即可）。
    let _ = cmd_tx.lock().unwrap().send(Command::Running(false));
}

// ------------------------------------------------------------------ 主题 ----

fn apply_theme(ctx: &egui::Context) {
    let mut visuals = Visuals::dark();

    visuals.panel_fill = PANEL_FILL;
    visuals.window_fill = CARD_FILL;
    visuals.faint_bg_color = INSET_FILL;
    visuals.extreme_bg_color = Color32::from_rgb(12, 13, 18);
    visuals.hyperlink_color = ACCENT_COLOR;

    visuals.widgets.noninteractive.bg_fill = INSET_FILL;
    visuals.widgets.noninteractive.fg_stroke.color = TEXT_MAIN;
    visuals.widgets.inactive.bg_fill = CARD_STROKE;
    visuals.widgets.inactive.fg_stroke.color = TEXT_MAIN;
    visuals.widgets.hovered.bg_fill = tint(ACCENT_COLOR, 0.35);
    visuals.widgets.hovered.fg_stroke.color = Color32::WHITE;
    visuals.widgets.active.bg_fill = ACCENT_COLOR;
    // 按下态文字用黑：亮青底色上黑字对比度约 6:1，白字只有约 1.5:1。
    visuals.widgets.active.fg_stroke.color = Color32::BLACK;
    visuals.selection.bg_fill = tint(ACCENT_COLOR, 0.35);
    visuals.selection.stroke.color = Color32::WHITE;

    ctx.set_visuals(visuals);

    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(14.0, 6.0);
        style.spacing.window_margin = Margin::same(16);
        style.visuals.widgets.noninteractive.corner_radius = CornerRadius::same(6);
        style.visuals.widgets.inactive.corner_radius = CornerRadius::same(6);
        style.visuals.widgets.hovered.corner_radius = CornerRadius::same(6);
        style.visuals.widgets.active.corner_radius = CornerRadius::same(6);
        style.visuals.window_corner_radius = CornerRadius::same(8);
        style.visuals.menu_corner_radius = CornerRadius::same(8);
        style
            .text_styles
            .insert(TextStyle::Heading, egui::FontId::proportional(15.0));
        style
            .text_styles
            .insert(TextStyle::Body, egui::FontId::proportional(13.5));
        style
            .text_styles
            .insert(TextStyle::Button, egui::FontId::proportional(13.0));
        style
            .text_styles
            .insert(TextStyle::Small, egui::FontId::proportional(11.0));
        style
            .text_styles
            .insert(TextStyle::Monospace, egui::FontId::monospace(11.0));
    });
}

// ------------------------------------------------------------------ UI ----

fn header(
    ui: &mut egui::Ui,
    snap: &Snapshot,
    icon_awake: &egui::TextureHandle,
    icon_guard: &egui::TextureHandle,
) {
    ui.horizontal(|ui| {
        // 真实渲染的应用图标（与托盘/exe 同源），随模式切换。
        let icon = match snap.mode {
            Mode::KeepScreenOn => icon_awake,
            Mode::ScreenOffGuard => icon_guard,
        };
        ui.add(egui::Image::new(icon).fit_to_exact_size(egui::vec2(26.0, 26.0)));
        ui.label(
            RichText::new("WinKeepAwake")
                .strong()
                .size(15.0)
                .color(TEXT_MAIN),
        );

        // 右侧状态：小圆点 + 文字（Win11 原生风格，不用胶囊徽标）。
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(status_label(snap.status))
                    .size(12.0)
                    .strong()
                    .color(status_color(snap.status)),
            );
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), egui::Sense::hover());
            ui.painter()
                .circle_filled(rect.center(), 3.5, status_color(snap.status));
        });
    });
}

/// 当前状态陈述区：模式行 + 一行数据说明 + 4px 细条（定时停止剩余并入同一行节奏）。
fn hero(ui: &mut egui::Ui, snap: &Snapshot) {
    let (mode_txt, mode_color) = match snap.mode {
        Mode::KeepScreenOn => ("防息屏 · 屏幕保持常亮", ACCENT_COLOR),
        Mode::ScreenOffGuard => ("息屏防睡 · 到点关屏，系统不休眠", GUARD_COLOR),
    };
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, mode_color);
        ui.label(RichText::new(mode_txt).size(13.0).strong().color(TEXT_MAIN));
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(format!("空闲 {:.1}s", snap.current_idle_ms as f64 / 1000.0))
                    .monospace()
                    .size(11.0)
                    .color(TEXT_DIM),
            );
        });
    });

    ui.add_space(6.0);

    match snap.mode {
        Mode::KeepScreenOn => {
            let frac = (snap.current_idle_ms as f32
                / ((snap.idle_threshold_secs as f32) * 1000.0).max(1.0))
            .clamp(0.0, 1.0);
            ui.label(
                RichText::new(format!(
                    "到点注入无感输入 · 已注入 {} 次",
                    snap.injections_done
                ))
                .size(11.0)
                .color(TEXT_FAINT),
            );
            ui.add(progress_bar(frac, ACCENT_COLOR));
        }
        Mode::ScreenOffGuard => {
            if snap.display_off_active {
                let target = (snap.display_off_mins * 60) as f32;
                let idle = snap.current_idle_ms as f32 / 1000.0;
                let remaining = (target - idle).max(0.0);
                if remaining > 0.0 {
                    ui.label(
                        RichText::new(format!(
                            "约 {} 分 {:02} 秒后关屏 · 系统保持满速运行",
                            (remaining as u64) / 60,
                            (remaining as u64) % 60
                        ))
                        .size(11.0)
                        .color(TEXT_DIM),
                    );
                    ui.add(progress_bar(
                        (idle / target.max(1.0)).clamp(0.0, 1.0),
                        GUARD_COLOR,
                    ));
                } else {
                    ui.label(
                        RichText::new("屏幕已关 · 系统保持满速运行")
                            .size(11.0)
                            .color(TEXT_DIM),
                    );
                    ui.add(progress_bar(1.0, GUARD_COLOR));
                }
            } else {
                // 写入失败的安全降级路径：显式横幅 + 原因，而不是一行小字。
                warn_banner(
                    ui,
                    "息屏防睡未生效：关屏超时写入失败",
                    &format!(
                        "已安全降级为屏幕常亮，系统依然保持唤醒。原因：{}",
                        snap.power_note.as_deref().unwrap_or("未知")
                    ),
                );
            }
        }
    }

    // 定时自动停止：剩余时间 + 总进度
    if let Some(remaining) = snap.auto_stop_remaining_secs {
        ui.add_space(6.0);
        let total = snap.auto_stop_total_secs.unwrap_or(remaining.max(1)) as f32;
        let frac = 1.0 - (remaining as f32 / total);
        ui.label(
            RichText::new(format!(
                "定时停止：剩余 {} 分 {:02} 秒",
                remaining / 60,
                remaining % 60
            ))
            .size(11.0)
            .color(AMBER),
        );
        ui.add(progress_bar(frac.clamp(0.0, 1.0), AMBER));
    }
}

/// 统一样式的细进度条（4px、微圆角、指定颜色填充）。
fn progress_bar(frac: f32, color: Color32) -> ProgressBar {
    ProgressBar::new(frac)
        .fill(color)
        .desired_height(4.0)
        .corner_radius(CornerRadius::same(2))
}

/// 模式设置区固定高度：两种模式的内容块高度不同，预留最大高度，
/// 切换模式时下方段落纹丝不动（息屏防睡的横幅正文压缩到单行以贴近防息屏块高度）。
const MODE_SETTINGS_MIN_HEIGHT: f32 = 104.0;

fn controls(ui: &mut egui::Ui, app: &mut MyApp) {
    // —— 模式分段选择器 + 运行主开关 ——
    segmented_mode(ui, app);
    ui.add_space(8.0);

    let is_running = app.snapshot_clone().status == Status::Running;
    let (btn_txt, btn_color) = if is_running {
        ("暂停保活", AMBER)
    } else {
        ("开始保活", GREEN)
    };
    let btn = egui::Button::new(RichText::new(btn_txt).strong().color(Color32::BLACK))
        .fill(btn_color)
        .min_size(Vec2::new(ui.available_width(), 32.0))
        .corner_radius(CornerRadius::same(6));
    if ui.add(btn).clicked() {
        app.send_cmd(Command::Running(!is_running));
    }

    // —— 模式设置：行式布局（左标签右控件），原生设置面板的样子 ——
    section_label(ui, "模式设置");
    let top = ui.cursor().top();
    match app.ui_mode {
        Mode::KeepScreenOn => {
            let mut changed = false;
            setting_row(ui, "空闲阈值", |ui| {
                let r = ui.add_sized(
                    [160.0, 22.0],
                    egui::Slider::new(&mut app.ui_idle_secs, 10..=300)
                        .step_by(5.0)
                        .suffix(" 秒"),
                );
                changed = r.changed();
                r
            });
            if changed {
                app.send_cmd(Command::IdleThresholdSecs(app.ui_idle_secs));
            }

            setting_row(ui, "注入方式", |ui| {
                ui.horizontal(|ui| {
                    let zero = ui
                        .selectable_label(
                            app.ui_mouse_mode == MouseMode::ZeroMove,
                            RichText::new("零位移").size(12.0),
                        )
                        .clicked();
                    let jiggle = ui
                        .selectable_label(
                            app.ui_mouse_mode == MouseMode::Jiggle,
                            RichText::new("±1px 抖动").size(12.0),
                        )
                        .clicked();
                    if zero && app.ui_mouse_mode != MouseMode::ZeroMove {
                        app.ui_mouse_mode = MouseMode::ZeroMove;
                        app.send_cmd(Command::MouseMode(MouseMode::ZeroMove));
                    } else if jiggle && app.ui_mouse_mode != MouseMode::Jiggle {
                        app.ui_mouse_mode = MouseMode::Jiggle;
                        app.send_cmd(Command::MouseMode(MouseMode::Jiggle));
                    }
                })
                .response
            })
            .on_hover_text(
                "零位移：光标完全不动，对绝大多数空闲检测有效；若系统不接受会自动降级为 ±1px 抖动。",
            );

            if ui
                .checkbox(&mut app.ui_keyboard_enabled, "键盘层 F13（一般不需要）")
                .on_hover_text("注入 F13 虚拟键：常见键盘没有这个键，几乎不会被任何软件绑定。")
                .changed()
            {
                app.send_cmd(Command::KeyboardEnabled(app.ui_keyboard_enabled));
            }
        }
        Mode::ScreenOffGuard => {
            let mut changed = false;
            setting_row(ui, "息屏等待", |ui| {
                let r = ui.add_sized(
                    [160.0, 22.0],
                    egui::Slider::new(&mut app.ui_display_off_mins, 1..=60).suffix(" 分钟"),
                );
                changed = r.changed();
                r
            });
            if changed {
                app.ui_mode_touched = true;
                app.send_cmd(Command::DisplayOffMins(app.ui_display_off_mins));
            }

            warn_banner(
                ui,
                "实验性：先白天短测，暂勿过夜依赖",
                "三次夜间挂死均发生在屏幕已关 + 系统保活在场的等价环境；过夜请用防息屏。",
            )
            .on_hover_text(
                "本机 2026-11 三次夜间锁屏挂死均发生在「屏幕已关 + PowerToys Awake 持 SYSTEM 声明」\
                 的等价环境，嫌疑是核显驱动的关屏低功耗转换。关屏超时临时写入电源计划\
                 （VIDEOIDLE/VIDEOCONLOCK AC/DC 四项），暂停/退出自动恢复，每 5 分钟防组策略漂移重应用。",
            );
        }
    }
    // 高度对齐：把较短的模式块补齐到统一高度，切换时零跳动。
    let used = ui.cursor().top() - top;
    ui.add_space((MODE_SETTINGS_MIN_HEIGHT - used).max(0.0));

    // —— 定时自动停止 ——
    section_label(ui, "定时自动停止");
    if ui
        .checkbox(&mut app.ui_auto_stop_enabled, "启用定时停止")
        .changed()
    {
        let dur = if app.ui_auto_stop_enabled {
            Some(Duration::from_secs(app.ui_auto_stop_mins * 60))
        } else {
            None
        };
        app.send_cmd(Command::AutoStop(dur));
    }
    ui.add_enabled_ui(app.ui_auto_stop_enabled, |ui| {
        let mut changed = false;
        setting_row(ui, "时长", |ui| {
            let r = ui.add_sized(
                [160.0, 22.0],
                egui::Slider::new(&mut app.ui_auto_stop_mins, 5..=480).suffix(" 分钟"),
            );
            changed = r.changed();
            r
        });
        if changed {
            app.send_cmd(Command::AutoStop(Some(Duration::from_secs(
                app.ui_auto_stop_mins * 60,
            ))));
        }
        if ui
            .checkbox(&mut app.ui_lock_on_stop, "到点立即锁屏")
            .on_hover_text(
                "仅释放电源声明并不会让屏幕立刻上锁（要等系统自己的息屏超时），\
                 开启后到点直接 LockWorkStation。",
            )
            .changed()
        {
            app.send_cmd(Command::SetLockOnStop(app.ui_lock_on_stop));
        }
    });

    // —— 通用 ——
    section_label(ui, "通用");
    ui.checkbox(
        &mut app.ui_minimize_on_close,
        "关闭窗口时最小化到托盘（而不是退出）",
    );
}

/// 设置行：左侧标签、右侧控件，垂直居中——原生设置面板的标准行式。
fn setting_row(
    ui: &mut egui::Ui,
    label: &str,
    right: impl FnOnce(&mut egui::Ui) -> egui::Response,
) -> egui::Response {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(12.5).color(TEXT_MAIN));
        ui.with_layout(egui::Layout::right_to_left(Align::Center), right)
            .inner
    })
    .response
}

/// 双模式分段选择器：整行两枚按钮，选中者填充模式色（青=防息屏，靛蓝=息屏防睡）。
fn segmented_mode(ui: &mut egui::Ui, app: &mut MyApp) {
    egui::Frame::new()
        .fill(INSET_FILL)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 8.0) / 2.0;
                let keep_selected = app.ui_mode == Mode::KeepScreenOn;
                let guard_selected = app.ui_mode == Mode::ScreenOffGuard;
                if seg_button(ui, "防息屏 · 屏幕常亮", width, keep_selected, ACCENT_COLOR).clicked()
                    && !keep_selected
                {
                    app.ui_mode_touched = true;
                    app.ui_mode = Mode::KeepScreenOn;
                    app.send_cmd(Command::SetMode(Mode::KeepScreenOn));
                }
                if seg_button(
                    ui,
                    "息屏防睡 · 关屏不休眠",
                    width,
                    guard_selected,
                    GUARD_COLOR,
                )
                .clicked()
                    && !guard_selected
                {
                    app.ui_mode_touched = true;
                    app.ui_mode = Mode::ScreenOffGuard;
                    app.send_cmd(Command::SetMode(Mode::ScreenOffGuard));
                }
            });
        });
}

/// 分段选择器里的一枚按钮：选中时填充模式色 + 深色字（亮底上黑字对比度最好）。
fn seg_button(
    ui: &mut egui::Ui,
    text: &str,
    width: f32,
    selected: bool,
    color: Color32,
) -> egui::Response {
    let txt = RichText::new(text).size(12.5).strong().color(if selected {
        Color32::BLACK
    } else {
        TEXT_DIM
    });
    let btn = egui::Button::new(txt)
        .min_size(Vec2::new(width, 30.0))
        .fill(if selected { color } else { INSET_FILL })
        .corner_radius(CornerRadius::same(4));
    ui.add(btn)
}

/// 小节标题：小号灰字 + 上下留白，不加装饰线（扁平设置面板风格）。
fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(text).size(11.0).strong().color(TEXT_FAINT));
    ui.add_space(4.0);
}

/// 橙色警示横幅：标题加粗 + 单行详情，返回 Response 供调用方挂悬停长文。
fn warn_banner(ui: &mut egui::Ui, title: &str, detail: &str) -> egui::Response {
    egui::Frame::new()
        .fill(tint(ORANGE, 0.1))
        .stroke(Stroke::new(1.0, tint(ORANGE, 0.35)))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).size(12.0).strong().color(ORANGE));
            if !detail.is_empty() {
                ui.label(RichText::new(detail).size(11.0).color(TEXT_DIM));
            }
        })
        .response
}

fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::KeepScreenOn => "防息屏",
        Mode::ScreenOffGuard => "息屏防睡",
    }
}

fn status_label(status: Status) -> &'static str {
    match status {
        Status::Running => "运行中",
        Status::Paused => "已暂停",
        Status::StoppedByTimer => "定时停止",
    }
}

fn status_color(status: Status) -> Color32 {
    match status {
        Status::Running => GREEN,
        Status::Paused => AMBER,
        Status::StoppedByTimer => TEXT_DIM,
    }
}

/// 日志区：窗口最后一个区块，吃掉全部剩余高度——默认窗口尺寸下日志完整可见。
fn log_section(ui: &mut egui::Ui, snap: &Snapshot) {
    section_label(ui, "日志");
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("时间")
                .monospace()
                .size(10.5)
                .color(TEXT_FAINT),
        );
        ui.label(
            RichText::new("事件")
                .monospace()
                .size(10.5)
                .color(TEXT_FAINT),
        );
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(format!("{} 条", snap.log.len()))
                    .monospace()
                    .size(10.5)
                    .color(TEXT_FAINT),
            );
        });
    });
    ui.add_space(2.0);
    let height = ui.available_height().max(72.0);
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height(height)
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for line in snap.log.iter() {
                let color = match line.level {
                    LogLevel::Info => Color32::from_rgb(195, 203, 220),
                    LogLevel::Warn => AMBER,
                    LogLevel::Error => RED,
                };
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(line.time.as_str())
                            .monospace()
                            .size(10.5)
                            .color(TEXT_FAINT),
                    );
                    ui.label(
                        RichText::new(line.msg.as_str())
                            .monospace()
                            .size(10.5)
                            .color(color),
                    );
                });
            }
            if snap.log.is_empty() {
                ui.label(
                    RichText::new("（暂无日志）")
                        .monospace()
                        .size(10.5)
                        .color(TEXT_FAINT),
                );
            }
        });
}

// ------------------------------------------------------------------ 托盘 + Win32 窗口句柄 ----

fn extract_hwnd(cc: &eframe::CreationContext<'_>) -> isize {
    match cc.window_handle() {
        Ok(handle) => match handle.as_raw() {
            RawWindowHandle::Win32(h) => h.hwnd.get(),
            other => {
                eprintln!("非预期的窗口句柄类型: {other:?}");
                0
            }
        },
        Err(e) => {
            eprintln!("获取窗口句柄失败: {e}");
            0
        }
    }
}

fn setup_tray(
    hwnd_raw: isize,
    cmd_tx: Arc<Mutex<Sender<Command>>>,
    snapshot: Arc<Mutex<Snapshot>>,
) -> Option<TrayIcon> {
    let rgba = build_icon_rgba(32);
    let icon = match Icon::from_rgba(rgba, 32, 32) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("托盘图标生成失败: {e}");
            return None;
        }
    };

    let menu = Menu::new();
    let show_item = MenuItem::with_id(MENU_ID_SHOW_HIDE, "显示 / 隐藏主窗口", true, None);
    let pause_item = MenuItem::with_id(MENU_ID_TOGGLE_PAUSE, "暂停 / 恢复保活", true, None);
    let restart_item = MenuItem::with_id(MENU_ID_RESTART, "重启工具", true, None);
    let sep = PredefinedMenuItem::separator();
    let quit_item = MenuItem::with_id(MENU_ID_QUIT, "退出", true, None);
    if menu
        .append_items(&[&show_item, &pause_item, &restart_item, &sep, &quit_item])
        .is_err()
    {
        eprintln!("托盘菜单构建失败");
        return None;
    }

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        // 左键留给「显示/隐藏主窗口」手势，菜单只由右键唤起
        // （tray-icon 0.26 默认 menu_on_left_click=true，即用户反馈的左键弹菜单问题）。
        .with_menu_on_left_click(false)
        .with_tooltip("WinKeepAwake · 防息屏守护")
        .with_icon(icon)
        .build();

    let tray = match tray {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("托盘图标创建失败（可能需要先有可见窗口）: {e}");
            return None;
        }
    };

    // 左键双击托盘图标：显示/隐藏主窗口（走原生 Win32 ShowWindow，绕开已知的
    // ViewportCommand::Visible 无法再次显示的边界问题）。
    let show_hwnd = hwnd_raw;
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        let is_left_activate = matches!(
            &event,
            TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } | TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
        );
        if is_left_activate {
            toggle_window_visibility(show_hwnd);
        }
    }));

    let cmd_tx_for_menu = cmd_tx.clone();
    let snapshot_for_menu = snapshot.clone();
    let quit_hwnd = hwnd_raw;
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        match event.id().0.as_str() {
            MENU_ID_SHOW_HIDE => toggle_window_visibility(quit_hwnd),
            MENU_ID_TOGGLE_PAUSE => {
                let running = {
                    let s = snapshot_for_menu.lock().unwrap();
                    s.status == Status::Running
                };
                let _ = cmd_tx_for_menu
                    .lock()
                    .unwrap()
                    .send(Command::Running(!running));
            }
            MENU_ID_RESTART => {
                // 重启工具：拉起新实例（息屏防睡生效中则用启动参数交接模式与等待时长），
                // 随后恢复电源计划并退出。新实例启动失败时保持当前实例运行——
                // 「退出后无人保护」比「继续跑旧实例」危险得多。
                let (guard_active, mins) = {
                    let s = snapshot_for_menu.lock().unwrap();
                    (s.display_off_active, s.display_off_mins)
                };
                let spawned = match std::env::current_exe() {
                    Ok(exe) => {
                        let mut cmd = std::process::Command::new(exe);
                        if guard_active {
                            cmd.arg("--screen-off-guard").arg(mins.to_string());
                        }
                        cmd.spawn().is_ok()
                    }
                    Err(_) => false,
                };
                if spawned {
                    crate::power::end_display_off();
                    crate::jitter::keep_awake_release();
                    std::process::exit(0);
                } else {
                    crate::worker::push_log(
                        &snapshot_for_menu,
                        LogLevel::Warn,
                        "重启失败：无法启动新实例，继续以当前实例运行",
                    );
                }
            }
            MENU_ID_QUIT => {
                crate::power::end_display_off();
                crate::jitter::keep_awake_release();
                std::process::exit(0);
            }
            _ => {}
        }
    }));

    tray
}

fn toggle_window_visibility(hwnd_raw: isize) {
    if hwnd_raw == 0 {
        return;
    }
    if window_is_visible(hwnd_raw) {
        window_hide(hwnd_raw);
    } else {
        window_show(hwnd_raw);
    }
}

// ------------------------------------------------------------------ main ----

fn main() -> eframe::Result {
    // 调试出口：--dump-icons <dir> 把两种模式的图标渲染成 BMP 后退出（视觉验收用）。
    // 产物是文件而非控制台输出，release（windows 子系统、无控制台）下同样可用。
    let argv: Vec<String> = std::env::args().collect();
    if let Some(pos) = argv.iter().position(|a| a == "--dump-icons") {
        let dir = argv.get(pos + 1).cloned().unwrap_or_else(|| ".".into());
        if let Err(e) = dump_icons(&dir) {
            eprintln!("--dump-icons 失败: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
    let snapshot = Arc::new(Mutex::new(Snapshot::default()));
    let _worker = worker::spawn_worker(cmd_rx, snapshot.clone());

    // 「重启工具」的状态交接：旧实例退出前以 `--screen-off-guard <mins>` 拉起新实例，
    // 新实例在 worker 起跑后立即恢复息屏防睡模式，中间只有约一秒的声明空窗（无风险）。
    let mut handoff_guard_mins: Option<u64> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--screen-off-guard" {
            handoff_guard_mins = Some(
                args.next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_DISPLAY_OFF_MINS)
                    .clamp(1, 60),
            );
        }
    }
    if let Some(mins) = handoff_guard_mins {
        let _ = cmd_tx.send(Command::DisplayOffMins(mins));
        let _ = cmd_tx.send(Command::SetMode(Mode::ScreenOffGuard));
    }

    let cmd_tx_shared = Arc::new(Mutex::new(cmd_tx));

    let rgba = build_icon_rgba(32);
    let icon_data = IconData {
        rgba,
        width: 32,
        height: 32,
    };

    let viewport = egui::ViewportBuilder::default()
        .with_app_id("com.winkeepawake.app")
        .with_title("WinKeepAwake · 防息屏守护")
        .with_inner_size([460.0, 840.0])
        .with_min_inner_size([420.0, 620.0])
        .with_icon(Arc::new(icon_data));

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let snapshot_for_creator = snapshot.clone();
    let cmd_tx_for_creator = cmd_tx_shared.clone();
    eframe::run_native(
        "WinKeepAwake",
        options,
        Box::new(move |cc| {
            Ok(Box::new(MyApp::new(
                cc,
                snapshot_for_creator,
                cmd_tx_for_creator,
            )))
        }),
    )
}

/// `--dump-icons`：两种模式图标 × {32, 256}px 渲染成 BMP，供人工视觉验收。
fn dump_icons(dir: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, variant) in [
        ("awake", IconVariant::Awake),
        ("nightguard", IconVariant::NightGuard),
    ] {
        for size in [32u32, 256] {
            let rgba = build_mode_icon_rgba(size, variant);
            let path = format!("{dir}\\icon-{name}-{size}.bmp");
            write_bmp(&path, size, &rgba)?;
        }
    }
    Ok(())
}

/// 手写 32bpp BMP（自底向上 BGRA），避免为一次性调试输出引入图像编码依赖。
fn write_bmp(path: &str, size: u32, rgba: &[u8]) -> std::io::Result<()> {
    let row = size * 4;
    let data_size = row * size;
    let mut out = Vec::with_capacity(54 + data_size as usize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(54u32 + data_size).to_le_bytes()); // 文件大小
    out.extend_from_slice(&0u32.to_le_bytes()); // 保留字段
    out.extend_from_slice(&54u32.to_le_bytes()); // 像素数据偏移
    out.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bpp
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&data_size.to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes()); // 72 DPI
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // 调色板
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            out.push(rgba[i + 2]); // B
            out.push(rgba[i + 1]); // G
            out.push(rgba[i]); // R
            out.push(rgba[i + 3]); // A
        }
    }
    std::fs::write(path, out)
}

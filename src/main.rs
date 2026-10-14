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
mod worker;

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{
    self, Align, Color32, CornerRadius, IconData, Margin, RichText, ScrollArea, Stroke, TextStyle,
    Vec2, Visuals,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

use crate::icon::build_icon_rgba;
use crate::jitter::{window_hide, window_is_visible, window_show, MouseMode};
use crate::worker::{Command, Snapshot, Status};

const DEFAULT_IDLE_SECS: u64 = 50;

const ACCENT: [u8; 3] = icon::ACCENT;
const ACCENT_DARK: [u8; 3] = icon::ACCENT_DARK;

const MENU_ID_SHOW_HIDE: &str = "show_hide";
const MENU_ID_TOGGLE_PAUSE: &str = "toggle_pause";
const MENU_ID_QUIT: &str = "quit";

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
}

impl MyApp {
    fn new(cc: &eframe::CreationContext<'_>, snapshot: Arc<Mutex<Snapshot>>, cmd_tx: Arc<Mutex<Sender<Command>>>) -> Self {
        apply_theme(&cc.egui_ctx);
        // 中文字形回退：egui 自带字体不含 CJK，不注入会出现"豆腐块"
        font::install_cjk_font(&cc.egui_ctx);

        let hwnd_raw = extract_hwnd(cc);

        let tray = setup_tray(hwnd_raw, cmd_tx.clone(), snapshot.clone());

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
            injections_done: s.injections_done,
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
                worker_keep_awake_release_via_cmd(&self.cmd_tx);
            }
        }

        let snap = self.snapshot_clone();

        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered_justified(|ui| {
                header(ui);
            });
            ui.add_space(12.0);
            status_card(ui, &snap);
            ui.add_space(12.0);
            controls(ui, self);
            ui.add_space(12.0);
            log_card(ui, &snap);
        });
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
    let accent = Color32::from_rgb(ACCENT[0], ACCENT[1], ACCENT[2]);
    let accent_dark = Color32::from_rgb(ACCENT_DARK[0], ACCENT_DARK[1], ACCENT_DARK[2]);

    visuals.panel_fill = Color32::from_rgb(17, 19, 26);
    visuals.window_fill = Color32::from_rgb(24, 26, 34);
    visuals.faint_bg_color = Color32::from_rgb(30, 32, 41);
    visuals.extreme_bg_color = Color32::from_rgb(12, 13, 18);
    visuals.hyperlink_color = accent;

    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(28, 30, 40);
    visuals.widgets.noninteractive.fg_stroke.color = Color32::from_rgb(214, 220, 232);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(42, 45, 58);
    visuals.widgets.inactive.fg_stroke.color = Color32::from_rgb(200, 206, 220);
    visuals.widgets.hovered.bg_fill = accent_dark;
    visuals.widgets.hovered.fg_stroke.color = Color32::WHITE;
    visuals.widgets.active.bg_fill = accent;
    visuals.widgets.active.fg_stroke.color = Color32::WHITE;
    visuals.selection.bg_fill = accent_dark;
    visuals.selection.stroke.color = Color32::WHITE;

    ctx.set_visuals(visuals);

    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = Vec2::new(10.0, 8.0);
        style.spacing.button_padding = Vec2::new(12.0, 7.0);
        style.spacing.window_margin = Margin::same(14);
        style.visuals.widgets.noninteractive.corner_radius = CornerRadius::same(8);
        style.visuals.widgets.inactive.corner_radius = CornerRadius::same(8);
        style.visuals.widgets.hovered.corner_radius = CornerRadius::same(8);
        style.visuals.widgets.active.corner_radius = CornerRadius::same(8);
        style.visuals.window_corner_radius = CornerRadius::same(14);
        style.visuals.menu_corner_radius = CornerRadius::same(10);
        style
            .text_styles
            .insert(TextStyle::Heading, egui::FontId::proportional(19.0));
        style
            .text_styles
            .insert(TextStyle::Body, egui::FontId::proportional(14.0));
        style
            .text_styles
            .insert(TextStyle::Button, egui::FontId::proportional(14.0));
        style
            .text_styles
            .insert(TextStyle::Small, egui::FontId::proportional(11.5));
    });
}

// ------------------------------------------------------------------ UI ----

fn header(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(26.0, 26.0), egui::Sense::hover());
        ui.painter().rect_filled(
            rect.shrink(2.0),
            CornerRadius::same(8),
            Color32::from_rgb(ACCENT[0], ACCENT[1], ACCENT[2]),
        );
        ui.painter().circle_filled(
            rect.center(),
            4.5,
            Color32::WHITE,
        );

        ui.vertical(|ui| {
            ui.label(
                RichText::new("WinKeepAwake")
                    .strong()
                    .size(19.0)
                    .color(Color32::from_rgb(235, 240, 250)),
            );
            ui.label(
                RichText::new("防息屏 / 防锁定 · 后台守护")
                    .size(12.0)
                    .color(Color32::from_rgb(140, 148, 165)),
            );
        });
    });
}

fn status_card(ui: &mut egui::Ui, snap: &Snapshot) {
    card(ui, |ui| {
        ui.horizontal(|ui| {
            let (color, text) = match snap.status {
                Status::Running => (Color32::from_rgb(52, 211, 153), "运行中"),
                Status::Paused => (Color32::from_rgb(251, 191, 36), "已暂停"),
                Status::StoppedByTimer => (Color32::from_rgb(148, 163, 184), "已自动停止"),
            };
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 5.0, color);
            ui.label(RichText::new(text).strong().color(color));

            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("空闲 {:.1}s", snap.current_idle_ms as f64 / 1000.0))
                        .size(12.5)
                        .color(Color32::from_rgb(160, 168, 185)),
                );
            });
        });

        ui.add_space(6.0);
        ui.label(
            RichText::new(format!(
                "阈值 {}s · 累计注入 {} 次",
                snap.idle_threshold_secs, snap.injections_done
            ))
            .size(12.0)
            .color(Color32::from_rgb(150, 158, 175)),
        );
        if snap.effective_mouse_mode == MouseMode::ZeroMove
            && snap.current_idle_ms >= (snap.idle_threshold_secs as u32) * 1_000
        {
            // 仅在临近触发时提示一次当前生效模式，避免常态下堆满提示
            ui.label(
                RichText::new("提示：若零位移未被系统接受，将自动降级为 ±1px 抖动")
                    .size(11.0)
                    .color(Color32::from_rgb(120, 130, 150)),
            );
        }
        if let Some(remaining) = snap.auto_stop_remaining_secs {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("定时停止：剩余 {} 分 {} 秒", remaining / 60, remaining % 60))
                    .size(12.5)
                    .color(Color32::from_rgb(251, 191, 36)),
            );
        }
    });
}

fn controls(ui: &mut egui::Ui, app: &mut MyApp) {
    card(ui, |ui| {
        // 运行/暂停 主开关
        let is_running = app.snapshot_clone().status == Status::Running;
        let btn = if is_running {
            egui::Button::new(RichText::new("暂停保活").strong()).fill(Color32::from_rgb(251, 191, 36))
        } else {
            egui::Button::new(RichText::new("开始保活").strong()).fill(Color32::from_rgb(52, 211, 153))
        };
        if ui.add(btn.min_size(Vec2::new(0.0, 34.0))).clicked() {
            app.send_cmd(Command::Running(!is_running));
        }

        ui.add_space(10.0);

        ui.label(RichText::new("空闲阈值（秒）").size(12.5));
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Slider::new(&mut app.ui_idle_secs, 10..=300)
                        .step_by(5.0)
                        .text(""),
                )
                .changed()
            {
                app.send_cmd(Command::IdleThresholdSecs(app.ui_idle_secs));
            }
            ui.label(format!("{}s", app.ui_idle_secs));
        });

        ui.add_space(6.0);

        ui.label(RichText::new("鼠标注入方式").size(12.5));
        ui.horizontal(|ui| {
            let zero = ui
                .selectable_label(app.ui_mouse_mode == MouseMode::ZeroMove, "零位移（无感）")
                .clicked();
            let jiggle = ui
                .selectable_label(app.ui_mouse_mode == MouseMode::Jiggle, "±1px 抖动")
                .clicked();
            if zero {
                app.ui_mouse_mode = MouseMode::ZeroMove;
                app.send_cmd(Command::MouseMode(MouseMode::ZeroMove));
            } else if jiggle {
                app.ui_mouse_mode = MouseMode::Jiggle;
                app.send_cmd(Command::MouseMode(MouseMode::Jiggle));
            }
        });

        ui.add_space(6.0);

        if ui
            .checkbox(&mut app.ui_keyboard_enabled, "启用键盘扫描层（F13，一般不需要）")
            .changed()
        {
            app.send_cmd(Command::KeyboardEnabled(app.ui_keyboard_enabled));
        }

        ui.add_space(6.0);

        ui.label(RichText::new("定时自动停止").size(12.5));
        ui.horizontal(|ui| {
            if ui
                .checkbox(&mut app.ui_auto_stop_enabled, "启用")
                .changed()
            {
                let dur = if app.ui_auto_stop_enabled {
                    Some(Duration::from_secs(app.ui_auto_stop_mins * 60))
                } else {
                    None
                };
                app.send_cmd(Command::AutoStop(dur));
            }
            let mut mins_changed = false;
            ui.add_enabled_ui(app.ui_auto_stop_enabled, |ui| {
                if ui
                    .add(egui::Slider::new(&mut app.ui_auto_stop_mins, 5..=480).suffix(" 分钟"))
                    .changed()
                {
                    mins_changed = true;
                }
            });
            if mins_changed {
                app.send_cmd(Command::AutoStop(Some(Duration::from_secs(
                    app.ui_auto_stop_mins * 60,
                ))));
            }
        });

        ui.add_enabled_ui(app.ui_auto_stop_enabled, |ui| {
            if ui
                .checkbox(
                    &mut app.ui_lock_on_stop,
                    "到点立即锁屏（否则仅停止保活，需等系统自己息屏）",
                )
                .changed()
            {
                app.send_cmd(Command::SetLockOnStop(app.ui_lock_on_stop));
            }
        });

        ui.add_space(6.0);
        ui.checkbox(
            &mut app.ui_minimize_on_close,
            "关闭窗口时最小化到托盘（而不是退出）",
        );
    });
}

fn log_card(ui: &mut egui::Ui, snap: &Snapshot) {
    card(ui, |ui| {
        ui.label(
            RichText::new("运行日志")
                .size(12.5)
                .color(Color32::from_rgb(150, 158, 175)),
        );
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(120.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in snap.log.iter() {
                    ui.label(RichText::new(line).size(11.5).color(Color32::from_rgb(190, 198, 214)));
                }
                if snap.log.is_empty() {
                    ui.label(
                        RichText::new("（暂无日志）")
                            .size(11.5)
                            .color(Color32::from_rgb(120, 128, 145)),
                    );
                }
            });
    });
}

/// 统一的"卡片"容器：圆角、轻微描边、内边距，营造现代分层感。
fn card<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(Color32::from_rgb(24, 26, 34))
        .stroke(Stroke::new(1.0, Color32::from_rgb(42, 45, 58)))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered_justified(|ui| add_contents(ui)).inner
        })
        .inner
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
    let sep = PredefinedMenuItem::separator();
    let quit_item = MenuItem::with_id(MENU_ID_QUIT, "退出", true, None);
    if menu.append_items(&[&show_item, &pause_item, &sep, &quit_item]).is_err() {
        eprintln!("托盘菜单构建失败");
        return None;
    }

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
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
            MENU_ID_QUIT => {
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
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
    let snapshot = Arc::new(Mutex::new(Snapshot::default()));
    let _worker = worker::spawn_worker(cmd_rx, snapshot.clone());
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
        .with_inner_size([420.0, 640.0])
        .with_min_inner_size([360.0, 480.0])
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

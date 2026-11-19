//! 后台保活工作线程：独立于 GUI 渲染循环，保证即使主窗口被隐藏到托盘、
//! egui 的 `App::update()` 不再被高频调用，防锁逻辑（空闲检测、注入、电源声明、定时自动停止）依然持续运行。

use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::jitter::{
    get_idle_ms, keep_awake_active, keep_awake_release, keep_awake_system_only, lock_workstation,
    perform_keyboard_injection, perform_mouse_injection, MouseMode,
};

/// GUI -> 工作线程 的单向命令通道。工作线程每个 tick 都会尝试排空并应用。
pub enum Command {
    Running(bool),
    IdleThresholdSecs(u64),
    MouseMode(MouseMode),
    KeyboardEnabled(bool),
    /// `Some(Duration)` = 从此刻起计时，到点后自动停止保活；`None` = 关闭定时停止功能。
    AutoStop(Option<Duration>),
    /// 定时停止到点时是否立即锁屏（`LockWorkStation`）。仅在启用定时停止时有意义。
    SetLockOnStop(bool),
    /// 切换工作模式（防息屏 / 息屏防睡）。
    SetMode(Mode),
    /// 息屏防睡模式下的「息屏等待」分钟数（写入时夹取到 1..=60）。
    DisplayOffMins(u64),
}

/// 工作模式。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    /// 防息屏（默认）：屏幕保持常亮。输入注入 + 完整电源声明（含 `ES_DISPLAY_REQUIRED`），
    /// 不触碰电源计划——即使域 GPO 重置系统设置，该模式依然有效（进程级声明不受 GPO 影响）。
    #[default]
    KeepScreenOn,
    /// 息屏防睡：到点正常关屏（含外接显示器），但系统保持满速唤醒、不进任何
    /// 待机/低功耗转换。**无输入注入**（注入会刷新显示空闲计时、阻止关屏），
    /// 电源声明只含 `ES_SYSTEM_REQUIRED`；同时临时把系统「关闭显示」超时改写为本模式
    /// 的等待时长，退出/暂停时恢复原值（安全不变量：允许关屏 ⟺ 本工具在保护）。
    ScreenOffGuard,
}

/// 日志级别（GUI 按级别着色，便于一眼扫出异常）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

/// 一条带时间戳与级别的运行日志。
#[derive(Clone, Debug)]
pub struct LogLine {
    pub time: String,
    pub level: LogLevel,
    pub msg: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Running,
    Paused,
    StoppedByTimer,
}

/// 工作线程对外发布的只读快照，GUI 每次重绘时读取展示。
pub struct Snapshot {
    pub status: Status,
    pub idle_threshold_secs: u64,
    pub current_idle_ms: u32,
    /// 最近一次实际生效的鼠标注入方式（`ZeroMove` 可能因运行时自检不通过而降级为 `Jiggle`）。
    pub effective_mouse_mode: MouseMode,
    /// 定时停止：距离到点还剩多少秒（未启用则为 `None`）。
    pub auto_stop_remaining_secs: Option<u64>,
    /// 定时停止：总时长秒数（用于进度条；未启用则为 `None`）。
    pub auto_stop_total_secs: Option<u64>,
    pub injections_done: u64,
    pub mode: Mode,
    /// 息屏防睡：息屏等待分钟数。
    pub display_off_mins: u64,
    /// 息屏防睡：电源计划改动当前是否生效（进入该模式且 powercfg 写入成功）。
    pub display_off_active: bool,
    /// 息屏防睡：电源计划操作失败原因（未生效时向 GUI 展示，屏幕保持常亮的安全降级）。
    pub power_note: Option<String>,
    pub log: VecDeque<LogLine>,
}

/// 息屏防睡的默认息屏等待（分钟）。
pub const DEFAULT_DISPLAY_OFF_MINS: u64 = 5;

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            status: Status::Running,
            idle_threshold_secs: 50,
            current_idle_ms: 0,
            effective_mouse_mode: MouseMode::ZeroMove,
            auto_stop_remaining_secs: None,
            auto_stop_total_secs: None,
            injections_done: 0,
            mode: Mode::KeepScreenOn,
            display_off_mins: DEFAULT_DISPLAY_OFF_MINS,
            display_off_active: false,
            power_note: None,
            log: VecDeque::new(),
        }
    }
}

const POLL_INTERVAL: Duration = Duration::from_millis(1_000);
const EXECUTION_STATE_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// 息屏防睡：电源设置漂移校验间隔（域 GPO 实测会周期性重置电源计划）。
const DISPLAY_OFF_VERIFY_INTERVAL: Duration = Duration::from_secs(300);
const MAX_LOG_LINES: usize = 120;

struct Config {
    running: bool,
    idle_threshold_secs: u64,
    mouse_mode: MouseMode,
    keyboard_enabled: bool,
    auto_stop_after: Option<Duration>,
    lock_on_stop: bool,
    mode: Mode,
    display_off_mins: u64,
}

/// 测试钩子：生产路径全部为 `None`（走真实 Win32 调用），单元测试里注入确定性行为，
/// 使工作线程的核心逻辑（命令应用、定时停止、注入节流/计数）可在毫秒级验证。
/// 钩子签名类型别名（仅服务于 TestHooks，降低嵌套 Box<dyn Fn> 的复杂度）
type IdleFn = Box<dyn Fn() -> Option<u32> + Send + Sync>;
type MouseFn = Box<dyn FnMut(MouseMode) -> (MouseMode, bool) + Send>;
type KbFn = Box<dyn Fn() -> bool + Send + Sync>;
type LockFn = Box<dyn Fn() -> bool + Send + Sync>;
type PowerBeginFn = Box<dyn FnMut(u32) -> Result<(), String> + Send>;
type PowerEndFn = Box<dyn FnMut() + Send>;
type PowerEffectiveFn = Box<dyn FnMut(u32) -> bool + Send>;

#[derive(Default)]
pub struct TestHooks {
    /// 覆盖 `jitter::get_idle_ms` 的返回值。
    pub idle_override: Option<IdleFn>,
    /// 覆盖鼠标注入（返回：实际生效方式, 是否被系统接受）。
    pub mouse_inject: Option<MouseFn>,
    /// 覆盖键盘注入。
    pub kb_inject: Option<KbFn>,
    /// 覆盖锁屏（单元测试注入，避免真的把开发机锁屏）。
    pub lock: Option<LockFn>,
    /// 覆盖 `power::begin_display_off`（单元测试绝不真改电源计划）。
    pub power_begin: Option<PowerBeginFn>,
    /// 覆盖 `power::end_display_off`。
    pub power_end: Option<PowerEndFn>,
    /// 覆盖 `power::display_off_effective`（漂移校验）。
    pub power_effective: Option<PowerEffectiveFn>,
}

/// 工作线程的运行参数（tick 间隔等），测试可缩短以便快速验证。
pub struct WorkerOpts {
    pub poll_interval: Duration,
    pub es_refresh_interval: Duration,
    pub power_verify_interval: Duration,
    pub hooks: TestHooks,
}

impl Default for WorkerOpts {
    fn default() -> Self {
        Self {
            poll_interval: POLL_INTERVAL,
            es_refresh_interval: EXECUTION_STATE_REFRESH_INTERVAL,
            power_verify_interval: DISPLAY_OFF_VERIFY_INTERVAL,
            hooks: TestHooks::default(),
        }
    }
}

/// 工作线程主体：循环 tick，直到收到进程退出（随进程一起结束，无需显式停止线程）。
pub fn spawn_worker(
    cmd_rx: Receiver<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
) -> thread::JoinHandle<()> {
    spawn_worker_with(cmd_rx, snapshot, WorkerOpts::default())
}

pub fn spawn_worker_with(
    cmd_rx: Receiver<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
    opts: WorkerOpts,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut worker = Worker::new(&snapshot);
        let mut opts = opts;
        let poll_interval = opts.poll_interval;
        loop {
            worker.tick(&cmd_rx, &snapshot, &mut opts);
            thread::sleep(poll_interval);
        }
    })
}

struct Worker {
    cfg: Config,
    last_jiggle_at: Option<Instant>,
    last_es_refresh: Instant,
    last_power_verify: Instant,
    auto_stop_started_at: Option<Instant>,
    injections_done: u64,
    status: Status,
    /// 息屏防睡：电源计划改动当前是否已写入生效。
    display_off_active: bool,
    /// 息屏防睡：最近一次电源计划操作失败原因（成功/未启用时为 `None`）。
    power_note: Option<String>,
}

impl Worker {
    fn new(snapshot: &Arc<Mutex<Snapshot>>) -> Self {
        let cfg = Config {
            running: true,
            idle_threshold_secs: snapshot.lock().unwrap().idle_threshold_secs,
            mouse_mode: MouseMode::ZeroMove,
            keyboard_enabled: true,
            auto_stop_after: None,
            lock_on_stop: false,
            mode: Mode::KeepScreenOn,
            display_off_mins: DEFAULT_DISPLAY_OFF_MINS,
        };

        // 启动即声明一次，避免刚运行就命中已有的息屏计时器。
        keep_awake_active();
        push_log(
            snapshot,
            LogLevel::Info,
            "保活已启动（鼠标零位移 + 电源管理声明）",
        );

        Self {
            cfg,
            last_jiggle_at: None,
            last_es_refresh: Instant::now(),
            last_power_verify: Instant::now(),
            auto_stop_started_at: None,
            injections_done: 0,
            status: Status::Running,
            display_off_active: false,
            power_note: None,
        }
    }

    /// 应用息屏防睡的电源计划改动（走真实 powercfg，或测试钩子）。
    fn power_begin(&mut self, hooks: &mut TestHooks) -> Result<(), String> {
        let secs = (self.cfg.display_off_mins as u32) * 60;
        match hooks.power_begin.as_mut() {
            Some(f) => f(secs),
            None => crate::power::begin_display_off(secs),
        }
    }

    /// 恢复电源计划改动（走真实 powercfg，或测试钩子）。
    fn power_end(&mut self, hooks: &mut TestHooks) {
        match hooks.power_end.as_mut() {
            Some(f) => f(),
            None => crate::power::end_display_off(),
        }
    }

    /// 校验息屏超时是否仍生效（走真实 powercfg，或测试钩子）。
    fn power_effective(&mut self, hooks: &mut TestHooks) -> bool {
        let secs = (self.cfg.display_off_mins as u32) * 60;
        match hooks.power_effective.as_mut() {
            Some(f) => f(secs),
            None => crate::power::display_off_effective(secs),
        }
    }

    /// 进入/重新应用息屏防睡：先切换电源声明（立即阻断空闲→低功耗转换的判定），
    /// 再改电源计划。powercfg 失败时安全降级为「屏幕常亮 + 系统唤醒」并把原因暴露给 GUI。
    fn enter_display_off_guard(&mut self, snapshot: &Arc<Mutex<Snapshot>>, hooks: &mut TestHooks) {
        keep_awake_system_only();
        match self.power_begin(hooks) {
            Ok(()) => {
                self.display_off_active = true;
                self.power_note = None;
                self.last_power_verify = Instant::now();
                push_log(
                    snapshot,
                    LogLevel::Info,
                    format!(
                        "息屏防睡已启用：{} 分钟无输入（或锁屏）后自动关屏；系统保持满速唤醒，不进低功耗转换",
                        self.cfg.display_off_mins
                    ),
                );
            }
            Err(e) => {
                self.display_off_active = false;
                self.power_note = Some(e.clone());
                push_log(
                    snapshot,
                    LogLevel::Error,
                    format!("电源计划修改失败，息屏防睡未生效（屏幕将保持常亮）：{e}"),
                );
            }
        }
    }

    /// 退出息屏防睡的电源计划改动（恢复原值）。未启用时是空操作。
    fn leave_display_off(&mut self, snapshot: &Arc<Mutex<Snapshot>>, hooks: &mut TestHooks) {
        if self.display_off_active {
            self.display_off_active = false;
            self.power_note = None;
            self.power_end(hooks);
            push_log(
                snapshot,
                LogLevel::Info,
                "已恢复电源计划（关屏超时还原为进入前状态）",
            );
        }
    }

    fn read_idle_ms(&self, hooks: &TestHooks) -> u32 {
        match hooks.idle_override {
            Some(ref f) => f().unwrap_or(0),
            None => get_idle_ms().unwrap_or(0),
        }
    }

    fn inject_mouse(&mut self, hooks: &mut TestHooks) -> (MouseMode, bool) {
        match hooks.mouse_inject {
            Some(ref mut f) => f(self.cfg.mouse_mode),
            None => perform_mouse_injection(self.cfg.mouse_mode),
        }
    }

    fn inject_keyboard(&self, hooks: &TestHooks) -> bool {
        match hooks.kb_inject {
            Some(ref f) => f(),
            None => perform_keyboard_injection(),
        }
    }

    fn lock_screen(&self, hooks: &TestHooks) -> bool {
        match hooks.lock {
            Some(ref f) => f(),
            None => lock_workstation(),
        }
    }

    /// 单次 tick：排空命令 → 定时停止判定 → 保活逻辑 → 刷新快照。
    fn tick(
        &mut self,
        cmd_rx: &Receiver<Command>,
        snapshot: &Arc<Mutex<Snapshot>>,
        opts: &mut WorkerOpts,
    ) {
        // 1. 排空并应用 GUI 发来的最新配置
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                Command::Running(v) => {
                    self.cfg.running = v;
                    if v {
                        self.status = Status::Running;
                        match self.cfg.mode {
                            Mode::KeepScreenOn => keep_awake_active(),
                            Mode::ScreenOffGuard => {
                                self.enter_display_off_guard(snapshot, &mut opts.hooks);
                            }
                        }
                        self.last_es_refresh = Instant::now();
                        push_log(snapshot, LogLevel::Info, "保活已恢复");
                    } else {
                        self.status = Status::Paused;
                        self.leave_display_off(snapshot, &mut opts.hooks);
                        keep_awake_release();
                        self.auto_stop_started_at = None;
                        push_log(
                            snapshot,
                            LogLevel::Info,
                            "保活已暂停（已释放电源管理声明，并恢复关屏超时）",
                        );
                    }
                    self.refresh(snapshot, None, None);
                }
                Command::IdleThresholdSecs(v) => {
                    self.cfg.idle_threshold_secs = v;
                    push_log(snapshot, LogLevel::Info, format!("空闲阈值调整为 {v}s"));
                    self.refresh(snapshot, None, None);
                }
                Command::MouseMode(m) => {
                    self.cfg.mouse_mode = m;
                    push_log(
                        snapshot,
                        LogLevel::Info,
                        format!(
                            "鼠标注入方式调整为{}",
                            match m {
                                MouseMode::ZeroMove => "零位移（无感）",
                                MouseMode::Jiggle => "±1px 抖动",
                            }
                        ),
                    );
                    self.refresh(snapshot, None, None);
                }
                Command::KeyboardEnabled(v) => {
                    self.cfg.keyboard_enabled = v;
                    push_log(
                        snapshot,
                        LogLevel::Info,
                        format!("键盘扫描层已{}", if v { "开启（F13）" } else { "关闭" }),
                    );
                    self.refresh(snapshot, None, None);
                }
                Command::AutoStop(dur) => {
                    self.cfg.auto_stop_after = dur;
                    self.auto_stop_started_at = dur.map(|_| Instant::now());
                    match dur {
                        Some(d) => push_log(
                            snapshot,
                            LogLevel::Info,
                            format!("定时停止已启用：{} 分钟后自动停止保活", d.as_secs() / 60),
                        ),
                        None => push_log(snapshot, LogLevel::Info, "定时停止已关闭"),
                    }
                    self.refresh(snapshot, None, None);
                }
                Command::SetLockOnStop(v) => {
                    self.cfg.lock_on_stop = v;
                    push_log(
                        snapshot,
                        LogLevel::Info,
                        format!("到点立即锁屏已{}", if v { "开启" } else { "关闭" }),
                    );
                    self.refresh(snapshot, None, None);
                }
                Command::SetMode(m) => {
                    if self.cfg.mode != m {
                        self.cfg.mode = m;
                        match m {
                            Mode::KeepScreenOn => {
                                self.leave_display_off(snapshot, &mut opts.hooks);
                                if self.cfg.running {
                                    keep_awake_active();
                                    push_log(
                                        snapshot,
                                        LogLevel::Info,
                                        "已切换为防息屏模式：屏幕保持常亮",
                                    );
                                } else {
                                    push_log(
                                        snapshot,
                                        LogLevel::Info,
                                        "已切换为防息屏模式（当前已暂停，恢复保活后生效）",
                                    );
                                }
                            }
                            Mode::ScreenOffGuard => {
                                if self.cfg.running {
                                    self.enter_display_off_guard(snapshot, &mut opts.hooks);
                                } else {
                                    push_log(
                                        snapshot,
                                        LogLevel::Info,
                                        "已切换为息屏防睡模式（当前已暂停，恢复保活后生效）",
                                    );
                                }
                            }
                        }
                    }
                    self.refresh(snapshot, None, None);
                }
                Command::DisplayOffMins(v) => {
                    let v = v.clamp(1, 60);
                    self.cfg.display_off_mins = v;
                    push_log(snapshot, LogLevel::Info, format!("息屏等待调整为 {v} 分钟"));
                    if self.cfg.mode == Mode::ScreenOffGuard && self.cfg.running {
                        if self.display_off_active {
                            // 已启用：仅更新超时（begin 幂等，不会覆盖原备份）
                            match self.power_begin(&mut opts.hooks) {
                                Ok(()) => self.last_power_verify = Instant::now(),
                                Err(e) => {
                                    self.display_off_active = false;
                                    self.power_note = Some(e.clone());
                                    push_log(
                                        snapshot,
                                        LogLevel::Error,
                                        format!("更新息屏超时失败：{e}"),
                                    );
                                }
                            }
                        } else {
                            // 之前启用失败：借改值机会立即重试
                            self.enter_display_off_guard(snapshot, &mut opts.hooks);
                        }
                    }
                    self.refresh(snapshot, None, None);
                }
            }
        }

        // 2. 定时停止判定（与 running 状态无关，到点就停）
        if let (Some(dur), Some(started)) = (self.cfg.auto_stop_after, self.auto_stop_started_at) {
            if self.status == Status::Running && started.elapsed() >= dur {
                self.status = Status::StoppedByTimer;
                self.cfg.running = false;
                self.leave_display_off(snapshot, &mut opts.hooks);
                keep_awake_release();
                self.auto_stop_started_at = None;
                if self.cfg.lock_on_stop {
                    let ok = self.lock_screen(&opts.hooks);
                    push_log(
                        snapshot,
                        LogLevel::Info,
                        if ok {
                            "已到达设定时间，保活自动停止并锁定屏幕"
                        } else {
                            "已到达设定时间，保活自动停止（锁屏请求未成功）"
                        },
                    );
                } else {
                    push_log(snapshot, LogLevel::Info, "已到达设定时间，保活自动停止");
                }
            }
        }

        // 3. 常规保活逻辑
        if self.cfg.running {
            if self.last_es_refresh.elapsed() >= opts.es_refresh_interval {
                match self.cfg.mode {
                    Mode::KeepScreenOn => keep_awake_active(),
                    Mode::ScreenOffGuard => keep_awake_system_only(),
                }
                self.last_es_refresh = Instant::now();
            }

            // 息屏防睡：周期校验关屏超时是否被外部改动（域 GPO 实测会重置电源计划），
            // 漂移即重新应用——校验失败本身不影响已生效的电源声明。
            if self.cfg.mode == Mode::ScreenOffGuard
                && self.display_off_active
                && self.last_power_verify.elapsed() >= opts.power_verify_interval
            {
                self.last_power_verify = Instant::now();
                if !self.power_effective(&mut opts.hooks) {
                    match self.power_begin(&mut opts.hooks) {
                        Ok(()) => push_log(
                            snapshot,
                            LogLevel::Warn,
                            "检测到关屏超时被外部改动（疑似组策略刷新），已重新应用",
                        ),
                        Err(e) => {
                            self.display_off_active = false;
                            self.power_note = Some(e.clone());
                            push_log(
                                snapshot,
                                LogLevel::Error,
                                format!("重新应用息屏超时失败：{e}"),
                            );
                        }
                    }
                }
            }

            let idle_ms = self.read_idle_ms(&opts.hooks);
            let threshold_ms = (self.cfg.idle_threshold_secs as u32) * 1_000;

            // 输入注入只属于防息屏模式：息屏防睡下任何注入都会刷新显示空闲计时、
            // 阻止屏幕关闭，与该模式的目的直接冲突。
            if self.cfg.mode == Mode::KeepScreenOn && idle_ms >= threshold_ms {
                let should_fire = match self.last_jiggle_at {
                    None => true,
                    Some(t) => t.elapsed() >= Duration::from_secs(self.cfg.idle_threshold_secs),
                };
                if should_fire {
                    let (effective_mode, ok) = self.inject_mouse(&mut opts.hooks);
                    let kb_ok = if self.cfg.keyboard_enabled {
                        self.inject_keyboard(&opts.hooks)
                    } else {
                        true
                    };
                    self.last_jiggle_at = Some(Instant::now());

                    if ok && kb_ok {
                        self.injections_done += 1;
                        let mode_label = match effective_mode {
                            MouseMode::ZeroMove => "零位移",
                            MouseMode::Jiggle => {
                                if self.cfg.mouse_mode == MouseMode::ZeroMove {
                                    "±1px 抖动（零位移被忽略，已自动降级）"
                                } else {
                                    "±1px 抖动"
                                }
                            }
                        };
                        push_log(
                            snapshot,
                            LogLevel::Info,
                            format!("空闲 {}s，注入鼠标({mode_label}) 保活生效", idle_ms / 1_000),
                        );
                    } else {
                        push_log(
                            snapshot,
                            LogLevel::Warn,
                            format!("空闲 {}s，但注入未被系统确认生效，请留意", idle_ms / 1_000),
                        );
                    }
                }
            }

            let remaining = self.auto_stop_remaining();
            self.refresh(snapshot, Some(idle_ms), remaining);
        } else {
            // 非运行状态也要周期性刷新快照，让 GUI 能读到最新 status
            let remaining = self.auto_stop_remaining();
            let idle = if opts.hooks.idle_override.is_some() {
                Some(self.read_idle_ms(&opts.hooks))
            } else {
                get_idle_ms()
            };
            self.refresh(snapshot, idle, remaining);
        }
    }

    fn auto_stop_remaining(&self) -> Option<u64> {
        match (self.cfg.auto_stop_after, self.auto_stop_started_at) {
            (Some(dur), Some(started)) => Some(dur.saturating_sub(started.elapsed()).as_secs()),
            _ => None,
        }
    }

    fn refresh(
        &self,
        snapshot: &Arc<Mutex<Snapshot>>,
        idle_ms: Option<u32>,
        auto_stop_remaining_secs: Option<u64>,
    ) {
        let mut s = snapshot.lock().unwrap();
        s.status = self.status;
        s.idle_threshold_secs = self.cfg.idle_threshold_secs;
        if let Some(v) = idle_ms {
            s.current_idle_ms = v;
        }
        s.effective_mouse_mode = self.cfg.mouse_mode;
        s.auto_stop_remaining_secs = auto_stop_remaining_secs;
        s.auto_stop_total_secs = self.cfg.auto_stop_after.map(|d| d.as_secs());
        s.injections_done = self.injections_done;
        s.mode = self.cfg.mode;
        s.display_off_mins = self.cfg.display_off_mins;
        s.display_off_active = self.display_off_active;
        s.power_note = self.power_note.clone();
    }
}

/// 向共享快照的日志环形缓冲追加一行（带时间戳与级别；GUI 托盘/退出路径也会用到）。
pub(crate) fn push_log(snapshot: &Arc<Mutex<Snapshot>>, level: LogLevel, msg: impl Into<String>) {
    let mut s = snapshot.lock().unwrap();
    if s.log.len() >= MAX_LOG_LINES {
        s.log.pop_front();
    }
    s.log.push_back(LogLine {
        time: crate::jitter::now_hms(),
        level,
        msg: msg.into(),
    });
}

// ------------------------------------------------------------------ 测试 ----

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::mpsc;

    fn fast_opts(idle_ms: u32) -> WorkerOpts {
        WorkerOpts {
            poll_interval: Duration::from_millis(10),
            es_refresh_interval: Duration::from_secs(3600),
            power_verify_interval: Duration::from_secs(3600),
            hooks: TestHooks {
                idle_override: Some(Box::new(move || Some(idle_ms))),
                mouse_inject: Some(Box::new(|m| (m, true))),
                kb_inject: Some(Box::new(|| true)),
                lock: None,
                power_begin: None,
                power_end: None,
                power_effective: None,
            },
        }
    }

    fn wait_until(snapshot: &Arc<Mutex<Snapshot>>, pred: impl Fn(&Snapshot) -> bool) -> bool {
        for _ in 0..300 {
            if pred(&snapshot.lock().unwrap()) {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn startup_sets_running_status() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let h = spawn_worker_with(rx, snap.clone(), fast_opts(0));
        assert!(wait_until(&snap, |s| !s.log.is_empty()));
        assert_eq!(snap.lock().unwrap().status, Status::Running);
        drop(tx);
        let _ = h;
    }

    #[test]
    fn commands_apply_to_snapshot() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let h = spawn_worker_with(rx, snap.clone(), fast_opts(0));
        tx.send(Command::IdleThresholdSecs(77)).unwrap();
        tx.send(Command::MouseMode(MouseMode::Jiggle)).unwrap();
        tx.send(Command::KeyboardEnabled(true)).unwrap();
        assert!(wait_until(&snap, |s| s.idle_threshold_secs == 77
            && s.effective_mouse_mode == MouseMode::Jiggle));
        drop(tx);
        let _ = h;
    }

    #[test]
    fn pause_and_resume_switch_status() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let h = spawn_worker_with(rx, snap.clone(), fast_opts(0));
        tx.send(Command::Running(false)).unwrap();
        assert!(wait_until(&snap, |s| s.status == Status::Paused));
        tx.send(Command::Running(true)).unwrap();
        assert!(wait_until(&snap, |s| s.status == Status::Running));
        drop(tx);
        let _ = h;
    }

    #[test]
    fn injection_fires_and_counts_when_idle() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        // 阈值 1s，空闲恒定 2000ms → 每个节流窗口都应触发一次注入
        let h = spawn_worker_with(rx, snap.clone(), fast_opts(2_000));
        tx.send(Command::IdleThresholdSecs(1)).unwrap();
        assert!(wait_until(&snap, |s| s.injections_done >= 2));
        let s = snap.lock().unwrap();
        assert_eq!(s.current_idle_ms, 2_000);
        assert!(s.log.iter().any(|l| l.msg.contains("保活生效")));
        drop(tx);
        let _ = h;
    }

    #[test]
    fn failed_injection_not_counted_but_logged() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let opts = WorkerOpts {
            poll_interval: Duration::from_millis(10),
            es_refresh_interval: Duration::from_secs(3600),
            power_verify_interval: Duration::from_secs(3600),
            hooks: TestHooks {
                idle_override: Some(Box::new(|| Some(9_000))),
                mouse_inject: Some(Box::new(|m| (m, false))),
                kb_inject: Some(Box::new(|| true)),
                lock: None,
                power_begin: None,
                power_end: None,
                power_effective: None,
            },
        };
        let h = spawn_worker_with(rx, snap.clone(), opts);
        tx.send(Command::IdleThresholdSecs(1)).unwrap();
        assert!(wait_until(&snap, |s| !s.log.is_empty() && s.log.len() >= 2));
        let s = snap.lock().unwrap();
        assert_eq!(s.injections_done, 0);
        assert!(s.log.iter().any(|l| l.msg.contains("未被系统确认")));
        drop(tx);
        let _ = h;
    }

    #[test]
    fn zero_move_fallback_logged_as_downgrade() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let opts = WorkerOpts {
            poll_interval: Duration::from_millis(10),
            es_refresh_interval: Duration::from_secs(3600),
            power_verify_interval: Duration::from_secs(3600),
            hooks: TestHooks {
                idle_override: Some(Box::new(|| Some(9_000))),
                // 模拟运行时自检失败：请求 ZeroMove 但实际以 Jiggle 生效
                mouse_inject: Some(Box::new(|_| (MouseMode::Jiggle, true))),
                kb_inject: Some(Box::new(|| true)),
                lock: None,
                power_begin: None,
                power_end: None,
                power_effective: None,
            },
        };
        let h = spawn_worker_with(rx, snap.clone(), opts);
        tx.send(Command::IdleThresholdSecs(1)).unwrap();
        assert!(wait_until(&snap, |s| s.injections_done >= 1));
        let s = snap.lock().unwrap();
        assert!(s
            .log
            .iter()
            .any(|l| l.msg.contains("保活生效") && l.msg.contains("自动降级")));
        drop(tx);
        let _ = h;
    }

    #[test]
    fn auto_stop_fires_at_deadline() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let h = spawn_worker_with(rx, snap.clone(), fast_opts(0));
        tx.send(Command::AutoStop(Some(Duration::from_millis(80))))
            .unwrap();
        // 倒计时先于到点可见（saturating_sub 后至少有一 tick 显示剩余秒数或 0）
        assert!(wait_until(&snap, |s| s.status == Status::StoppedByTimer));
        let s = snap.lock().unwrap();
        assert!(s.auto_stop_remaining_secs.is_none());
        assert!(s.log.iter().any(|l| l.msg.contains("自动停止")));
        drop(tx);
        let _ = h;
    }

    #[test]
    fn auto_stop_locks_when_enabled() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let locked = Arc::new(AtomicBool::new(false));
        let locked2 = locked.clone();
        let opts = WorkerOpts {
            poll_interval: Duration::from_millis(10),
            es_refresh_interval: Duration::from_secs(3600),
            power_verify_interval: Duration::from_secs(3600),
            hooks: TestHooks {
                idle_override: Some(Box::new(|| Some(0))),
                mouse_inject: Some(Box::new(|m| (m, true))),
                kb_inject: Some(Box::new(|| true)),
                // 用钩子拦截锁屏，绝不真正锁开发机
                lock: Some(Box::new(move || {
                    locked2.store(true, Ordering::SeqCst);
                    true
                })),
                power_begin: None,
                power_end: None,
                power_effective: None,
            },
        };
        let h = spawn_worker_with(rx, snap.clone(), opts);
        tx.send(Command::SetLockOnStop(true)).unwrap();
        tx.send(Command::AutoStop(Some(Duration::from_millis(80))))
            .unwrap();
        assert!(wait_until(&snap, |s| s.status == Status::StoppedByTimer));
        assert!(wait_until(&snap, |_| locked.load(Ordering::SeqCst)));
        let s = snap.lock().unwrap();
        assert!(s.log.iter().any(|l| l.msg.contains("锁定屏幕")));
        drop(tx);
        let _ = h;
    }

    #[test]
    fn auto_stop_does_not_lock_when_disabled() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let locked = Arc::new(AtomicBool::new(false));
        let locked2 = locked.clone();
        let opts = WorkerOpts {
            poll_interval: Duration::from_millis(10),
            es_refresh_interval: Duration::from_secs(3600),
            power_verify_interval: Duration::from_secs(3600),
            hooks: TestHooks {
                idle_override: Some(Box::new(|| Some(0))),
                mouse_inject: Some(Box::new(|m| (m, true))),
                kb_inject: Some(Box::new(|| true)),
                lock: Some(Box::new(move || {
                    locked2.store(true, Ordering::SeqCst);
                    true
                })),
                power_begin: None,
                power_end: None,
                power_effective: None,
            },
        };
        let h = spawn_worker_with(rx, snap.clone(), opts);
        // 不开 SetLockOnStop，默认应不锁屏
        tx.send(Command::AutoStop(Some(Duration::from_millis(80))))
            .unwrap();
        assert!(wait_until(&snap, |s| s.status == Status::StoppedByTimer));
        thread::sleep(Duration::from_millis(60));
        assert!(
            !locked.load(Ordering::SeqCst),
            "未开启到点锁屏，不应触发锁屏"
        );
        drop(tx);
        let _ = h;
    }

    #[test]
    fn auto_stop_can_be_disabled_again() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let h = spawn_worker_with(rx, snap.clone(), fast_opts(0));
        tx.send(Command::AutoStop(Some(Duration::from_secs(3600))))
            .unwrap();
        assert!(wait_until(&snap, |s| s.auto_stop_remaining_secs.is_some()));
        tx.send(Command::AutoStop(None)).unwrap();
        assert!(wait_until(&snap, |s| s.auto_stop_remaining_secs.is_none()));
        drop(tx);
        let _ = h;
    }

    #[test]
    fn log_ring_buffer_caps_at_max_lines() {
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let pushed = MAX_LOG_LINES as u64 + 30;
        for i in 0..pushed {
            push_log(&snap, LogLevel::Info, format!("line-{i}"));
        }
        let s = snap.lock().unwrap();
        assert_eq!(s.log.len(), MAX_LOG_LINES);
        // 旧行从队首丢弃，新行从队尾进入：保留区间 [pushed-MAX, pushed-1]
        let expected_front = format!("line-{}", pushed - MAX_LOG_LINES as u64);
        let expected_back = format!("line-{}", pushed - 1);
        assert_eq!(s.log.front().unwrap().msg, expected_front);
        assert_eq!(s.log.back().unwrap().msg, expected_back);
        // 每条日志都应带时间戳与级别
        assert_eq!(s.log.front().unwrap().time.len(), 8);
        assert_eq!(s.log.front().unwrap().level, LogLevel::Info);
    }

    #[test]
    fn throttle_allows_single_injection_per_window() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let count = Arc::new(AtomicU32::new(0));
        let count2 = count.clone();
        let opts = WorkerOpts {
            poll_interval: Duration::from_millis(5),
            es_refresh_interval: Duration::from_secs(3600),
            power_verify_interval: Duration::from_secs(3600),
            hooks: TestHooks {
                idle_override: Some(Box::new(|| Some(60_000))),
                mouse_inject: Some(Box::new(move |m| {
                    count2.fetch_add(1, Ordering::SeqCst);
                    (m, true)
                })),
                kb_inject: None,
                lock: None,
                power_begin: None,
                power_end: None,
                power_effective: None,
            },
        };
        let h = spawn_worker_with(rx, snap.clone(), opts);
        // 阈值 60s → 节流窗口 60s，观察窗口内只允许第一次触发
        tx.send(Command::IdleThresholdSecs(60)).unwrap();
        thread::sleep(Duration::from_millis(300));
        assert_eq!(count.load(Ordering::SeqCst), 1, "节流失效：重复注入");
        drop(tx);
        let _ = h;
    }

    // ---------------------- 息屏防睡模式 ----------------------

    /// 构造带电源钩子的 opts（真实路径绝不触碰电源计划）。
    fn guard_opts<B, E, F>(begin: B, end: E, effective: F) -> WorkerOpts
    where
        B: FnMut(u32) -> Result<(), String> + Send + 'static,
        E: FnMut() + Send + 'static,
        F: FnMut(u32) -> bool + Send + 'static,
    {
        WorkerOpts {
            poll_interval: Duration::from_millis(10),
            es_refresh_interval: Duration::from_secs(3600),
            power_verify_interval: Duration::from_secs(3600),
            hooks: TestHooks {
                idle_override: Some(Box::new(|| Some(0))),
                mouse_inject: Some(Box::new(|m| (m, true))),
                kb_inject: Some(Box::new(|| true)),
                lock: None,
                power_begin: Some(Box::new(begin)),
                power_end: Some(Box::new(end)),
                power_effective: Some(Box::new(effective)),
            },
        }
    }

    #[test]
    fn screen_off_guard_activates_power_and_pause_restores() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let begins = Arc::new(Mutex::new(Vec::new()));
        let ends = Arc::new(Mutex::new(0u32));
        let opts = guard_opts(
            {
                let begins = begins.clone();
                move |secs| {
                    begins.lock().unwrap().push(secs);
                    Ok(())
                }
            },
            {
                let ends = ends.clone();
                move || {
                    *ends.lock().unwrap() += 1;
                }
            },
            |_| true,
        );
        let h = spawn_worker_with(rx, snap.clone(), opts);
        tx.send(Command::SetMode(Mode::ScreenOffGuard)).unwrap();
        assert!(wait_until(&snap, |s| s.mode == Mode::ScreenOffGuard
            && s.display_off_active));
        assert_eq!(
            *begins.lock().unwrap(),
            vec![(DEFAULT_DISPLAY_OFF_MINS * 60) as u32]
        );
        assert_eq!(*ends.lock().unwrap(), 0);
        // 暂停必须同时恢复电源计划（安全不变量：允许关屏 ⟺ 本工具在保护）
        tx.send(Command::Running(false)).unwrap();
        assert!(wait_until(&snap, |s| s.status == Status::Paused
            && !s.display_off_active));
        assert_eq!(*ends.lock().unwrap(), 1);
        drop(tx);
        let _ = h;
    }

    #[test]
    fn switching_back_to_keep_screen_on_restores_power() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let ends = Arc::new(Mutex::new(0u32));
        let opts = guard_opts(
            |_| Ok(()),
            {
                let ends = ends.clone();
                move || {
                    *ends.lock().unwrap() += 1;
                }
            },
            |_| true,
        );
        let h = spawn_worker_with(rx, snap.clone(), opts);
        tx.send(Command::SetMode(Mode::ScreenOffGuard)).unwrap();
        assert!(wait_until(&snap, |s| s.display_off_active));
        tx.send(Command::SetMode(Mode::KeepScreenOn)).unwrap();
        assert!(wait_until(&snap, |s| s.mode == Mode::KeepScreenOn
            && !s.display_off_active));
        assert_eq!(*ends.lock().unwrap(), 1);
        drop(tx);
        let _ = h;
    }

    #[test]
    fn auto_stop_in_guard_mode_restores_power() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let ends = Arc::new(Mutex::new(0u32));
        let opts = guard_opts(
            |_| Ok(()),
            {
                let ends = ends.clone();
                move || {
                    *ends.lock().unwrap() += 1;
                }
            },
            |_| true,
        );
        let h = spawn_worker_with(rx, snap.clone(), opts);
        tx.send(Command::SetMode(Mode::ScreenOffGuard)).unwrap();
        assert!(wait_until(&snap, |s| s.display_off_active));
        tx.send(Command::AutoStop(Some(Duration::from_millis(80))))
            .unwrap();
        assert!(wait_until(&snap, |s| s.status == Status::StoppedByTimer));
        assert!(wait_until(&snap, |s| !s.display_off_active));
        assert_eq!(*ends.lock().unwrap(), 1);
        drop(tx);
        let _ = h;
    }

    #[test]
    fn screen_off_guard_suppresses_injection() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        // 空闲恒 120s、阈值 1s：防息屏模式下必然持续注入；切到息屏防睡后必须停
        let opts = WorkerOpts {
            poll_interval: Duration::from_millis(10),
            es_refresh_interval: Duration::from_secs(3600),
            power_verify_interval: Duration::from_secs(3600),
            hooks: TestHooks {
                idle_override: Some(Box::new(|| Some(120_000))),
                mouse_inject: Some(Box::new(|m| (m, true))),
                kb_inject: Some(Box::new(|| true)),
                lock: None,
                power_begin: Some(Box::new(|_| Ok(()))),
                power_end: Some(Box::new(|| {})),
                power_effective: Some(Box::new(|_| true)),
            },
        };
        let h = spawn_worker_with(rx, snap.clone(), opts);
        tx.send(Command::IdleThresholdSecs(1)).unwrap();
        assert!(wait_until(&snap, |s| s.injections_done >= 1));
        let before = snap.lock().unwrap().injections_done;
        tx.send(Command::SetMode(Mode::ScreenOffGuard)).unwrap();
        assert!(wait_until(&snap, |s| s.display_off_active));
        thread::sleep(Duration::from_millis(250));
        assert_eq!(
            snap.lock().unwrap().injections_done,
            before,
            "息屏防睡下不应有任何输入注入（会阻止屏幕关闭）"
        );
        drop(tx);
        let _ = h;
    }

    #[test]
    fn power_failure_reported_and_retried_on_change() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let fail_first = Arc::new(AtomicBool::new(true));
        let begins = Arc::new(Mutex::new(Vec::new()));
        let opts = guard_opts(
            {
                let fail_first = fail_first.clone();
                let begins = begins.clone();
                move |secs| {
                    if fail_first.swap(false, Ordering::SeqCst) {
                        Err("拒绝访问".to_string())
                    } else {
                        begins.lock().unwrap().push(secs);
                        Ok(())
                    }
                }
            },
            || {},
            |_| true,
        );
        let h = spawn_worker_with(rx, snap.clone(), opts);
        tx.send(Command::SetMode(Mode::ScreenOffGuard)).unwrap();
        // 首次启用失败：保持安全降级（屏幕常亮），原因暴露到快照
        assert!(wait_until(&snap, |s| s.mode == Mode::ScreenOffGuard
            && !s.display_off_active
            && s.power_note.is_some()));
        {
            let s = snap.lock().unwrap();
            assert!(s.log.iter().any(|l| l.msg.contains("未生效")));
        }
        // 修改息屏等待应触发重试并成功（10 分钟 → 600 秒）
        tx.send(Command::DisplayOffMins(10)).unwrap();
        assert!(wait_until(&snap, |s| s.display_off_active && s.display_off_mins == 10));
        assert_eq!(*begins.lock().unwrap(), vec![600]);
        drop(tx);
        let _ = h;
    }

    #[test]
    fn drift_reapplies_display_off() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let begins = Arc::new(Mutex::new(Vec::new()));
        let mut opts = guard_opts(
            {
                let begins = begins.clone();
                move |secs| {
                    begins.lock().unwrap().push(secs);
                    Ok(())
                }
            },
            || {},
            |_| false, // 模拟每次校验都发现超时被组策略改掉
        );
        opts.power_verify_interval = Duration::from_millis(50);
        let h = spawn_worker_with(rx, snap.clone(), opts);
        tx.send(Command::SetMode(Mode::ScreenOffGuard)).unwrap();
        assert!(wait_until(&snap, |s| s.display_off_active));
        assert!(wait_until(&snap, |_| begins.lock().unwrap().len() >= 2));
        assert!(wait_until(&snap, |s| s
            .log
            .iter()
            .any(|l| l.msg.contains("重新应用"))));
        drop(tx);
        let _ = h;
    }
}

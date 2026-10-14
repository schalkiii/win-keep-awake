//! 后台保活工作线程：独立于 GUI 渲染循环，保证即使主窗口被隐藏到托盘、
//! egui 的 `App::update()` 不再被高频调用，防锁逻辑（空闲检测、注入、电源声明、定时自动停止）依然持续运行。

use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::jitter::{
    get_idle_ms, keep_awake_active, keep_awake_release, lock_workstation,
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
    pub injections_done: u64,
    pub log: VecDeque<String>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            status: Status::Running,
            idle_threshold_secs: 50,
            current_idle_ms: 0,
            effective_mouse_mode: MouseMode::ZeroMove,
            auto_stop_remaining_secs: None,
            injections_done: 0,
            log: VecDeque::new(),
        }
    }
}

const POLL_INTERVAL: Duration = Duration::from_millis(1_000);
const EXECUTION_STATE_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const MAX_LOG_LINES: usize = 120;

struct Config {
    running: bool,
    idle_threshold_secs: u64,
    mouse_mode: MouseMode,
    keyboard_enabled: bool,
    auto_stop_after: Option<Duration>,
    lock_on_stop: bool,
}

/// 测试钩子：生产路径全部为 `None`（走真实 Win32 调用），单元测试里注入确定性行为，
/// 使工作线程的核心逻辑（命令应用、定时停止、注入节流/计数）可在毫秒级验证。
/// 钩子签名类型别名（仅服务于 TestHooks，降低嵌套 Box<dyn Fn> 的复杂度）
type IdleFn = Box<dyn Fn() -> Option<u32> + Send + Sync>;
type MouseFn = Box<dyn FnMut(MouseMode) -> (MouseMode, bool) + Send>;
type KbFn = Box<dyn Fn() -> bool + Send + Sync>;
type LockFn = Box<dyn Fn() -> bool + Send + Sync>;

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
}

/// 工作线程的运行参数（tick 间隔等），测试可缩短以便快速验证。
pub struct WorkerOpts {
    pub poll_interval: Duration,
    pub es_refresh_interval: Duration,
    pub hooks: TestHooks,
}

impl Default for WorkerOpts {
    fn default() -> Self {
        Self {
            poll_interval: POLL_INTERVAL,
            es_refresh_interval: EXECUTION_STATE_REFRESH_INTERVAL,
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
    auto_stop_started_at: Option<Instant>,
    injections_done: u64,
    status: Status,
}

impl Worker {
    fn new(snapshot: &Arc<Mutex<Snapshot>>) -> Self {
        let cfg = Config {
            running: true,
            idle_threshold_secs: snapshot.lock().unwrap().idle_threshold_secs,
            mouse_mode: MouseMode::ZeroMove,
            keyboard_enabled: false,
            auto_stop_after: None,
            lock_on_stop: false,
        };

        // 启动即声明一次，避免刚运行就命中已有的息屏计时器。
        keep_awake_active();
        push_log(
            snapshot,
            "保活已启动（鼠标零位移 + 电源管理声明）".to_string(),
        );

        Self {
            cfg,
            last_jiggle_at: None,
            last_es_refresh: Instant::now(),
            auto_stop_started_at: None,
            injections_done: 0,
            status: Status::Running,
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
    fn tick(&mut self, cmd_rx: &Receiver<Command>, snapshot: &Arc<Mutex<Snapshot>>, opts: &mut WorkerOpts) {
        // 1. 排空并应用 GUI 发来的最新配置
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                Command::Running(v) => {
                    self.cfg.running = v;
                    if v {
                        self.status = Status::Running;
                        keep_awake_active();
                        self.last_es_refresh = Instant::now();
                        push_log(snapshot, "保活已恢复".to_string());
                    } else {
                        self.status = Status::Paused;
                        keep_awake_release();
                        self.auto_stop_started_at = None;
                        push_log(snapshot, "保活已暂停（已释放电源管理声明）".to_string());
                    }
                    self.refresh(snapshot, None, None);
                }
                Command::IdleThresholdSecs(v) => {
                    self.cfg.idle_threshold_secs = v;
                    push_log(snapshot, format!("空闲阈值调整为 {v}s"));
                    self.refresh(snapshot, None, None);
                }
                Command::MouseMode(m) => {
                    self.cfg.mouse_mode = m;
                    push_log(
                        snapshot,
                        format!(
                            "鼠标注入方式调整为 {}",
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
                            format!(
                                "定时停止已启用：{} 分钟后自动停止保活",
                                d.as_secs() / 60
                            ),
                        ),
                        None => push_log(snapshot, "定时停止已关闭".to_string()),
                    }
                    self.refresh(snapshot, None, None);
                }
                Command::SetLockOnStop(v) => {
                    self.cfg.lock_on_stop = v;
                    push_log(
                        snapshot,
                        format!("到点立即锁屏已{}", if v { "开启" } else { "关闭" }),
                    );
                    self.refresh(snapshot, None, None);
                }
            }
        }

        // 2. 定时停止判定（与 running 状态无关，到点就停）
        if let (Some(dur), Some(started)) = (self.cfg.auto_stop_after, self.auto_stop_started_at) {
            if self.status == Status::Running && started.elapsed() >= dur {
                self.status = Status::StoppedByTimer;
                self.cfg.running = false;
                keep_awake_release();
                self.auto_stop_started_at = None;
                if self.cfg.lock_on_stop {
                    let ok = self.lock_screen(&opts.hooks);
                    push_log(
                        snapshot,
                        if ok {
                            "已到达设定时间，保活自动停止并锁定屏幕".to_string()
                        } else {
                            "已到达设定时间，保活自动停止（锁屏请求未成功）".to_string()
                        },
                    );
                } else {
                    push_log(snapshot, "已到达设定时间，保活自动停止".to_string());
                }
            }
        }

        // 3. 常规保活逻辑
        if self.cfg.running {
            if self.last_es_refresh.elapsed() >= opts.es_refresh_interval {
                keep_awake_active();
                self.last_es_refresh = Instant::now();
            }

            let idle_ms = self.read_idle_ms(&opts.hooks);
            let threshold_ms = (self.cfg.idle_threshold_secs as u32) * 1_000;

            if idle_ms >= threshold_ms {
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
                            format!("空闲 {}s，注入鼠标({mode_label}) 保活生效", idle_ms / 1_000),
                        );
                    } else {
                        push_log(
                            snapshot,
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
        refresh_snapshot(
            snapshot,
            self.status,
            &self.cfg,
            idle_ms,
            auto_stop_remaining_secs,
            self.injections_done,
        );
    }
}

fn refresh_snapshot(
    snapshot: &Arc<Mutex<Snapshot>>,
    status: Status,
    cfg: &Config,
    idle_ms: Option<u32>,
    auto_stop_remaining_secs: Option<u64>,
    injections_done: u64,
) {
    let mut s = snapshot.lock().unwrap();
    s.status = status;
    s.idle_threshold_secs = cfg.idle_threshold_secs;
    if let Some(v) = idle_ms {
        s.current_idle_ms = v;
    }
    s.effective_mouse_mode = cfg.mouse_mode;
    s.auto_stop_remaining_secs = auto_stop_remaining_secs;
    s.injections_done = injections_done;
}

fn push_log(snapshot: &Arc<Mutex<Snapshot>>, line: String) {
    let mut s = snapshot.lock().unwrap();
    if s.log.len() >= MAX_LOG_LINES {
        s.log.pop_front();
    }
    s.log.push_back(line);
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
            hooks: TestHooks {
                idle_override: Some(Box::new(move || Some(idle_ms))),
                mouse_inject: Some(Box::new(|m| (m, true))),
                kb_inject: Some(Box::new(|| true)),
                lock: None,
            },
        }
    }

    fn wait_until(
        snapshot: &Arc<Mutex<Snapshot>>,
        pred: impl Fn(&Snapshot) -> bool,
    ) -> bool {
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
        assert!(s.log.iter().any(|l| l.contains("保活生效")));
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
            hooks: TestHooks {
                idle_override: Some(Box::new(|| Some(9_000))),
                mouse_inject: Some(Box::new(|m| (m, false))),
                kb_inject: None,
                lock: None,
            },
        };
        let h = spawn_worker_with(rx, snap.clone(), opts);
        tx.send(Command::IdleThresholdSecs(1)).unwrap();
        assert!(wait_until(&snap, |s| !s.log.is_empty() && s.log.len() >= 2));
        let s = snap.lock().unwrap();
        assert_eq!(s.injections_done, 0);
        assert!(s.log.iter().any(|l| l.contains("未被系统确认")));
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
            hooks: TestHooks {
                idle_override: Some(Box::new(|| Some(9_000))),
                // 模拟运行时自检失败：请求 ZeroMove 但实际以 Jiggle 生效
                mouse_inject: Some(Box::new(|_| (MouseMode::Jiggle, true))),
                kb_inject: None,
                lock: None,
            },
        };
        let h = spawn_worker_with(rx, snap.clone(), opts);
        tx.send(Command::IdleThresholdSecs(1)).unwrap();
        assert!(wait_until(&snap, |s| s.injections_done >= 1));
        let s = snap.lock().unwrap();
        assert!(s
            .log
            .iter()
            .any(|l| l.contains("保活生效") && l.contains("自动降级")));
        drop(tx);
        let _ = h;
    }

    #[test]
    fn auto_stop_fires_at_deadline() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let h = spawn_worker_with(rx, snap.clone(), fast_opts(0));
        tx.send(Command::AutoStop(Some(Duration::from_millis(80)))).unwrap();
        // 倒计时先于到点可见（saturating_sub 后至少有一 tick 显示剩余秒数或 0）
        assert!(wait_until(&snap, |s| s.status == Status::StoppedByTimer));
        let s = snap.lock().unwrap();
        assert!(s.auto_stop_remaining_secs.is_none());
        assert!(s.log.iter().any(|l| l.contains("自动停止")));
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
            hooks: TestHooks {
                idle_override: Some(Box::new(|| Some(0))),
                mouse_inject: Some(Box::new(|m| (m, true))),
                kb_inject: Some(Box::new(|| true)),
                // 用钩子拦截锁屏，绝不真正锁开发机
                lock: Some(Box::new(move || {
                    locked2.store(true, Ordering::SeqCst);
                    true
                })),
            },
        };
        let h = spawn_worker_with(rx, snap.clone(), opts);
        tx.send(Command::SetLockOnStop(true)).unwrap();
        tx.send(Command::AutoStop(Some(Duration::from_millis(80)))).unwrap();
        assert!(wait_until(&snap, |s| s.status == Status::StoppedByTimer));
        assert!(wait_until(&snap, |_| locked.load(Ordering::SeqCst)));
        let s = snap.lock().unwrap();
        assert!(s.log.iter().any(|l| l.contains("锁定屏幕")));
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
            hooks: TestHooks {
                idle_override: Some(Box::new(|| Some(0))),
                mouse_inject: Some(Box::new(|m| (m, true))),
                kb_inject: Some(Box::new(|| true)),
                lock: Some(Box::new(move || {
                    locked2.store(true, Ordering::SeqCst);
                    true
                })),
            },
        };
        let h = spawn_worker_with(rx, snap.clone(), opts);
        // 不开 SetLockOnStop，默认应不锁屏
        tx.send(Command::AutoStop(Some(Duration::from_millis(80)))).unwrap();
        assert!(wait_until(&snap, |s| s.status == Status::StoppedByTimer));
        thread::sleep(Duration::from_millis(60));
        assert!(!locked.load(Ordering::SeqCst), "未开启到点锁屏，不应触发锁屏");
        drop(tx);
        let _ = h;
    }

    #[test]
    fn auto_stop_can_be_disabled_again() {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let h = spawn_worker_with(rx, snap.clone(), fast_opts(0));
        tx.send(Command::AutoStop(Some(Duration::from_secs(3600)))).unwrap();
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
            push_log(&snap, format!("line-{i}"));
        }
        let s = snap.lock().unwrap();
        assert_eq!(s.log.len(), MAX_LOG_LINES);
        // 旧行从队首丢弃，新行从队尾进入：保留区间 [pushed-MAX, pushed-1]
        let expected_front = format!("line-{}", pushed - MAX_LOG_LINES as u64);
        let expected_back = format!("line-{}", pushed - 1);
        assert_eq!(s.log.front().unwrap(), &expected_front);
        assert_eq!(s.log.back().unwrap(), &expected_back);
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
            hooks: TestHooks {
                idle_override: Some(Box::new(|| Some(60_000))),
                mouse_inject: Some(Box::new(move |m| {
                    count2.fetch_add(1, Ordering::SeqCst);
                    (m, true)
                })),
                kb_inject: None,
                lock: None,
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
}

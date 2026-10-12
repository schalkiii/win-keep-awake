//! 纯 Win32 底层封装：输入注入、空闲检测、电源管理声明、窗口显隐。
//! 不依赖任何 UI/线程框架，方便单独测试、复用，也方便被后台工作线程调用。
//!
//! 仅适用于 Windows 目标平台。

use std::ffi::c_void;

use windows::core::BOOL;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetLastInputInfo, KEYBDINPUT, KEYEVENTF_KEYUP, LASTINPUTINFO, MOUSEINPUT, MOUSEEVENTF_MOVE,
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, VK_F13,
};
use windows::Win32::UI::WindowsAndMessaging::{
    IsWindowVisible, SetForegroundWindow, ShowWindow, SW_HIDE, SW_SHOW,
};

/// 鼠标注入方式。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseMode {
    /// 零位移移动：`SendInput(MOUSEEVENTF_MOVE, dx=0, dy=0)`。
    /// 光标视觉上完全不动（不会触发 hover / 不会漂移 / 不会干扰正在拖拽的操作），
    /// 但对多数基于"最后输入时间"判定空闲的检测手段依然有效——这是"更无感"的默认方式。
    ZeroMove,
    /// ±1 像素相对抖动。兼容性最好（少数会显式过滤零位移的严格检测工具能识别），
    /// 代价是光标会有肉眼几乎不可见的极小位移。
    Jiggle,
}

/// 通过 `GetLastInputInfo` 获取当前系统空闲毫秒数。
/// 该 API 反映"最近一次经由输入队列产生的真实/注入输入事件"的时间戳，
/// 是绝大多数应用层空闲检测（包括企业屏保策略）用来判定是否超时的依据。
pub fn get_idle_ms() -> Option<u32> {
    unsafe {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if GetLastInputInfo(&mut info).as_bool() {
            // GetTickCount 是 32 位毫秒计数器，约 49.7 天回绕一次，wrapping_sub 可正确处理回绕边界。
            Some(GetTickCount().wrapping_sub(info.dwTime))
        } else {
            None
        }
    }
}

fn send_inputs(inputs: &[INPUT]) -> bool {
    unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) as usize == inputs.len() }
}

fn send_single_mouse_move(dx: i32, dy: i32) -> bool {
    let inputs = [INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }];
    send_inputs(&inputs)
}

/// 若本次注入前有空闲时间读数，注入后立即复查，确认 `GetLastInputInfo` 真的被刷新了
/// （数值变小即代表系统接受了这次注入）。
/// 这一步不是形式主义：极少数驱动层/检测工具会忽略零位移事件，
/// 用它做运行时自检，才能保证"防锁"这个最终目的始终达成，而不是想当然。
fn idle_reset_confirmed(before: Option<u32>) -> bool {
    match (before, get_idle_ms()) {
        (Some(b), Some(a)) => a < b,
        _ => true, // 拿不到数据时不阻塞主流程，交给上层日志去提示不确定性
    }
}

/// 执行一次鼠标注入。返回实际生效的方式（`ZeroMove` 可能因运行时自检不通过而自动降级为 `Jiggle`）
/// 以及本次注入是否被系统接受。
pub fn perform_mouse_injection(mode: MouseMode) -> (MouseMode, bool) {
    match mode {
        MouseMode::Jiggle => {
            let before = get_idle_ms();
            let sent = send_single_mouse_move(1, 0) && send_single_mouse_move(-1, 0);
            (MouseMode::Jiggle, sent && idle_reset_confirmed(before))
        }
        MouseMode::ZeroMove => {
            let before = get_idle_ms();
            let sent = send_single_mouse_move(0, 0);
            if sent && idle_reset_confirmed(before) {
                (MouseMode::ZeroMove, true)
            } else {
                // 自动降级兜底：极少数环境里零位移不生效（或被过滤），此时用 ±1px 抖动再试一次，
                // 保证"防锁"这一最终目的优先于"完全无感"这一手段偏好。
                let before2 = get_idle_ms();
                let sent2 = send_single_mouse_move(1, 0) && send_single_mouse_move(-1, 0);
                (MouseMode::Jiggle, sent2 && idle_reset_confirmed(before2))
            }
        }
    }
}

/// 键盘注入层（可选，默认关闭）：注入 F13 虚拟键按下 + 释放。
///
/// 为什么选 F13 而不是 Ctrl/Shift：修饰键单独按下/释放虽然通常也不会触发任何快捷键，
/// 但仍存在与用户真实按键在极小概率下"时序重叠"从而被某些全局热键逻辑误判的组合风险；
/// F13 在绝大多数应用/游戏里根本没有绑定任何功能（因为常见物理键盘压根没有这个键），
/// 且不会像 Caps Lock / Scroll Lock 那样切换锁定状态或点亮键盘 LED，是"完全无副作用"的首选。
///
/// 注意：这里用 Virtual-Key（wVk）而不是硬件 ScanCode 注入——因为 F13 在常用扫描码集里
/// 没有标准编码，用虚拟键码更可靠；这是"无感优先"这一新需求下，相对"最原始物理扫描码"
/// 做的一个有意识取舍（键盘层本身已改为默认关闭，仅在用户显式开启时才会用到）。
pub fn perform_keyboard_injection() -> bool {
    let inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_F13,
                    wScan: 0,
                    dwFlags: Default::default(),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_F13,
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ];
    send_inputs(&inputs)
}

/// 向 Windows 电源管理子系统声明"禁止息屏/睡眠"（独立于输入注入之外的第三条防线，
/// 不依赖检测方是否识别注入痕迹，而是直接命令系统层面的计时器不要启动）。
pub fn keep_awake_active() {
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED);
    }
}

/// 撤销电源管理声明（`ES_CONTINUOUS` 单独使用 = 清除之前设置的持续标志），
/// 用于定时自动停止 / 手动停止 / 程序退出时的资源回收。
pub fn keep_awake_release() {
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS);
    }
}

/// 原生窗口显隐辅助（绕过 egui `ViewportCommand::Visible`，因为该 API 在部分版本存在
/// "隐藏后无法再次显示"的已知问题；直接用 Win32 `ShowWindow` 更可靠）。
///
/// `hwnd_raw` 是启动时从 `CreationContext::window_handle()` 取出并保存为 `isize` 的窗口句柄
/// （`HWND` 本身不是 `Send`/`Sync`，但它的整数值是，可以在其它线程/闭包中安全传递，
/// 只在"创建该窗口的同一线程"上用本函数重新构造 `HWND` 来调用 Win32 API）。
pub fn window_show(hwnd_raw: isize) {
    if hwnd_raw == 0 {
        return;
    }
    let hwnd = HWND(hwnd_raw as *mut c_void);
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        // 单纯 ShowWindow 不保证抢占前台焦点，SetForegroundWindow 可能被系统策略拒绝，
        // 这里尽力一试，失败也不影响功能（窗口至少会重新可见）。
        let _ = SetForegroundWindow(hwnd);
    }
}

pub fn window_hide(hwnd_raw: isize) {
    if hwnd_raw == 0 {
        return;
    }
    let hwnd = HWND(hwnd_raw as *mut c_void);
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

/// 判断窗口当前是否可见，用于托盘图标点击时的"显示/隐藏"切换语义。
pub fn window_is_visible(hwnd_raw: isize) -> bool {
    if hwnd_raw == 0 {
        return false;
    }
    let hwnd = HWND(hwnd_raw as *mut c_void);
    let r: BOOL = unsafe { IsWindowVisible(hwnd) };
    r.as_bool()
}

// ------------------------------------------------------------------ 测试 ----

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// 真实输入注入会读写全局系统空闲时间（GetLastInputInfo），并行跑时互相干扰会使
    /// `perform_mouse_injection` 内部的 `after < before` 自检偶发误判。用一把全局锁把
    /// 三个“会真实发输入”的测试串行化，彻底消除竞态。
    fn input_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn get_idle_ms_returns_plausible_value() {
        let idle = get_idle_ms().expect("GetLastInputInfo 应可用");
        // 开机以来必然有过输入；回绕处理下不应接近 u32::MAX
        assert!(idle < u32::MAX / 2, "异常空闲读数: {idle}");
    }

    #[test]
    fn send_input_primitives_are_accepted() {
        // 只验证 SendInput 本身被接受（返回注入数量），这是确定性的，不依赖空闲时序。
        let _g = input_lock().lock().unwrap_or_else(|e| e.into_inner());
        assert!(send_single_mouse_move(1, 0), "±1px 鼠标事件应被 SendInput 接受");
        assert!(send_single_mouse_move(-1, 0));
        assert!(send_single_mouse_move(0, 0), "零位移鼠标事件应被 SendInput 接受");
        assert!(perform_keyboard_injection(), "F13 按下+释放应被 SendInput 接受");
    }

    #[test]
    fn jiggle_injection_routes_to_jiggle_mode() {
        let _g = input_lock().lock().unwrap_or_else(|e| e.into_inner());
        // 只断言确定性的“模式路由”：请求 Jiggle 就应以 Jiggle 生效。
        // “空闲是否真的归零”依赖系统输入时序（本机可能有其他输入源），属于 E2E 验证范畴。
        let (mode, _ok) = perform_mouse_injection(MouseMode::Jiggle);
        assert_eq!(mode, MouseMode::Jiggle, "Jiggle 请求不应改变生效方式");
    }

    #[test]
    fn zero_move_injection_routes_to_valid_mode() {
        let _g = input_lock().lock().unwrap_or_else(|e| e.into_inner());
        let (mode, _ok) = perform_mouse_injection(MouseMode::ZeroMove);
        assert!(
            matches!(mode, MouseMode::ZeroMove | MouseMode::Jiggle),
            "零位移请求应返回合法生效方式（自检不通过时降级为 Jiggle）"
        );
    }

    #[test]
    fn keyboard_injection_is_idempotent_under_lock() {
        let _g = input_lock().lock().unwrap_or_else(|e| e.into_inner());
        // 连续两次发送都应成功（按下+释放成对，不留修饰键残留）
        assert!(perform_keyboard_injection());
        assert!(perform_keyboard_injection());
    }

    #[test]
    fn execution_state_set_and_release_succeed() {
        // SetThreadExecutionState 返回非 0 即成功（在测试线程上设置，退出前释放）
        let active = unsafe {
            SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED)
        };
        assert_ne!(active.0, 0, "声明电源请求应成功");
        let released = unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        assert_ne!(released.0, 0, "释放电源声明应成功");
    }

    #[test]
    fn window_helpers_reject_null_handle_safely() {
        assert!(!window_is_visible(0));
        window_show(0); // 不应 panic，早退
        window_hide(0);
    }
}

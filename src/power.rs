//! 电源计划封装（powercfg）：为「息屏防睡」模式动态启用/恢复「关闭显示超时」。
//!
//! 设计原则：**允许关屏必须与「保持唤醒」声明同生共死**。
//! 本机夜间挂死的根因是「关屏 + 空闲 → 低功耗转换中途冻死」；因此关屏超时
//! 绝不能被永久留在系统里——只在工具以息屏防睡模式运行期间临时写入，
//! 暂停/切回常亮/退出时立即恢复原值，让「工具不在运行」的世界回到
//! 「从不关屏」的安全基线（本机 2026-11-18 诊断结论，详见 README）。
//!
//! 另一层现实约束：本机电源计划疑似受域 GPO 周期管理（实测 2026-11-18，
//! 手工写入的 0「从不」在数小时内被改回默认 600s/300s）。因此除写入外
//! 还提供 [`display_off_effective`]，由 worker 周期调用做漂移校验，
//! 被外部改动即重新应用。
//!
//! 权限：`powercfg -set*index` 对当前电源计划无需管理员（已在本机实测验证），
//! 与 GUI 程序普通权限运行的身份一致。

use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::Mutex;

/// 子进程不弹控制台窗口（GUI 程序里 powercfg 一闪而过很难看）。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 「在此时间后关闭」（常规空闲关屏超时）。
pub const VIDEOIDLE: &str = "VIDEOIDLE";
/// 「控制台锁定显示关闭超时」（锁屏后多久关屏）——隐藏设置，用 GUID 直取。
pub const VIDEOCONLOCK: &str = "8EC4B3A5-6868-48c2-BE75-4F3044BE88A7";

/// 一组关屏超时（交流/直流，秒）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DisplayTimeouts {
    pub ac_secs: u32,
    pub dc_secs: u32,
}

/// 进入息屏防睡前的原始值备份（VIDEOIDLE, VIDEOCONLOCK）。`None` = 当前未启用。
static BACKUP: Mutex<Option<(DisplayTimeouts, DisplayTimeouts)>> = Mutex::new(None);

fn run_powercfg(args: &[&str]) -> Result<String, String> {
    let out = Command::new("powercfg")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("无法启动 powercfg: {e}"))?;
    if !out.status.success() {
        let stderr = decode(&out.stderr);
        let stdout = decode(&out.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        return Err(format!("powercfg {} 失败: {}", args.join(" "), detail));
    }
    Ok(decode(&out.stdout))
}

/// powercfg 管道输出兼容解码：UTF-16LE(BOM) / UTF-8(BOM) / 其余按 UTF-8 有损。
/// （本地代码页如 GBK 的中文会变乱码，但我们解析的 0x 十六进制是 ASCII，不受影响。）
fn decode(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    } else if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(&bytes[3..]).into_owned()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// 从 `powercfg -q` 单设置查询输出解析「当前交流/直流电源设置索引」。
/// 输出里还带「最小/最大/增量可能设置」等其它 0x 值——当前索引永远排在最后两个。
pub fn parse_current_indexes(output: &str) -> Option<DisplayTimeouts> {
    let hexes: Vec<u32> = output
        .split_whitespace()
        .filter_map(|tok| {
            let h = tok.strip_prefix("0x").or_else(|| tok.strip_prefix("0X"))?;
            // 容忍值后紧贴标点（如 "0x0000000F;"）：只取前缀连续十六进制位
            let end = h.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(h.len());
            u32::from_str_radix(&h[..end], 16).ok()
        })
        .collect();
    if hexes.len() < 2 {
        return None;
    }
    let n = hexes.len();
    Some(DisplayTimeouts {
        ac_secs: hexes[n - 2],
        dc_secs: hexes[n - 1],
    })
}

/// 读取指定显示设置的当前 AC/DC 值。查询/解析失败返回 `None`（调用方按 0 兜底并记日志）。
pub fn read_display_timeouts(setting: &str) -> Option<DisplayTimeouts> {
    let out = run_powercfg(&["-q", "SCHEME_CURRENT", "SUB_VIDEO", setting]).ok()?;
    parse_current_indexes(&out)
}

fn write_display_timeouts(setting: &str, t: DisplayTimeouts) -> Result<(), String> {
    run_powercfg(&[
        "-setacvalueindex",
        "SCHEME_CURRENT",
        "SUB_VIDEO",
        setting,
        &t.ac_secs.to_string(),
    ])?;
    run_powercfg(&[
        "-setdcvalueindex",
        "SCHEME_CURRENT",
        "SUB_VIDEO",
        setting,
        &t.dc_secs.to_string(),
    ])?;
    run_powercfg(&["-setactive", "SCHEME_CURRENT"]).map(|_| ())
}

/// 启用息屏防睡：备份当前关屏超时，然后把空闲/锁屏两处超时（AC/DC）都设为 `secs`。
/// 已有备份时不覆盖——避免把「自己写入的值」误存为原始值（重复启用/漂移重应用场景）。
pub fn begin_display_off(secs: u32) -> Result<(), String> {
    let mut backup = BACKUP.lock().unwrap_or_else(|e| e.into_inner());
    if backup.is_none() {
        let idle = read_display_timeouts(VIDEOIDLE).unwrap_or_default();
        let conlock = read_display_timeouts(VIDEOCONLOCK).unwrap_or_default();
        *backup = Some((idle, conlock));
    }
    let t = DisplayTimeouts {
        ac_secs: secs,
        dc_secs: secs,
    };
    write_display_timeouts(VIDEOIDLE, t)?;
    write_display_timeouts(VIDEOCONLOCK, t)
}

/// 恢复原值（暂停/切回常亮模式/退出/重启交接时调用）。幂等：未启用时空操作。
pub fn end_display_off() {
    let mut backup = BACKUP.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((idle, conlock)) = backup.take() {
        let _ = write_display_timeouts(VIDEOIDLE, idle);
        let _ = write_display_timeouts(VIDEOCONLOCK, conlock);
    }
}

/// 校验当前生效值是否仍等于 `secs`（域 GPO 实测会周期性重置电源设置）。
/// 返回 `false` 表示被外部改动，调用方应重新 [`begin_display_off`]。
pub fn display_off_effective(secs: u32) -> bool {
    let want = DisplayTimeouts {
        ac_secs: secs,
        dc_secs: secs,
    };
    let ok_idle = read_display_timeouts(VIDEOIDLE).is_some_and(|t| t == want);
    let ok_conlock = read_display_timeouts(VIDEOCONLOCK).is_some_and(|t| t == want);
    ok_idle && ok_conlock
}

// ------------------------------------------------------------------ 测试 ----

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// 两个真机测试都会真实读写电源计划（且 `begin/end` 共享静态 `BACKUP`），
    /// 默认并行跑会互踩：把对方的写入误认成「原值」备份，污染系统基线
    /// （2026-11-19 实测复现）。用全局锁强制串行——与 jitter.rs 的 input_lock 同一手法。
    fn real_power_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// 本机 powercfg -q 实际输出样例（中文标签 GBK/UTF-8 皆可，只解析 0x 值）。
    const SAMPLE_QUERY: &str = "\
电源设置 GUID: 3c0bc021-c8a8-4e07-a973-6b14cbcb2b7e  (在此时间后关闭)
      最小可能的设置: 0x00000000
      最大可能的设置: 0xffffffff
      可能的设置增量: 0x00000001
    当前交流电源设置索引: 0x00000258
    当前直流电源设置索引: 0x0000012c
";

    #[test]
    fn parse_takes_last_two_hex_indexes() {
        let t = parse_current_indexes(SAMPLE_QUERY).unwrap();
        assert_eq!(t.ac_secs, 0x258);
        assert_eq!(t.dc_secs, 0x12c);
    }

    #[test]
    fn parse_handles_plain_two_values() {
        let t = parse_current_indexes("a 0x0000003C b 0x00000000").unwrap();
        assert_eq!(t.ac_secs, 60);
        assert_eq!(t.dc_secs, 0);
    }

    #[test]
    fn parse_rejects_insufficient_tokens() {
        assert!(parse_current_indexes("").is_none());
        assert!(parse_current_indexes("没有十六进制值").is_none());
        assert!(parse_current_indexes("只有一个 0x0000000A").is_none());
    }

    #[test]
    fn parse_tolerates_trailing_punctuation() {
        // 行尾紧跟中文标点的防御性样例
        let t = parse_current_indexes("x: 0x0000000F; y: 0x0000001E。").unwrap();
        assert_eq!(t.ac_secs, 15);
        assert_eq!(t.dc_secs, 30);
    }

    #[test]
    fn decode_utf16le_bom() {
        let text = "abc 0x00000001";
        let mut bytes = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode(&bytes), text);
    }

    #[test]
    fn decode_utf8_bom_and_plain() {
        assert_eq!(decode(&[0xEF, 0xBB, 0xBF, b'h', b'i']), "hi");
        assert_eq!(decode(b"plain"), "plain");
    }

    /// 真实 powercfg 往返（会临时改写 VIDEOIDLE 并还原）——需要真机环境，
    /// 平时跳过：`cargo test -- --ignored` 手动跑。
    #[test]
    #[ignore = "真实修改电源设置，仅在目标机器上手动验证"]
    fn real_powercfg_roundtrip() {
        let _g = real_power_lock().lock().unwrap_or_else(|e| e.into_inner());
        let original = read_display_timeouts(VIDEOIDLE).expect("应能读到当前关屏超时");
        let probe = DisplayTimeouts {
            ac_secs: 555,
            dc_secs: 555,
        };
        write_display_timeouts(VIDEOIDLE, probe).expect("写入应成功（普通权限即可）");
        assert_eq!(read_display_timeouts(VIDEOIDLE).unwrap(), probe);
        write_display_timeouts(VIDEOIDLE, original).expect("应能还原");
        assert_eq!(read_display_timeouts(VIDEOIDLE).unwrap(), original);
    }

    /// 备份语义真机验证（安全不变量的核心分支）：重复 `begin` 不覆盖原始备份，
    /// `end` 恢复的是**最初**原值而非第一次写入的临时值。模拟「息屏防睡运行期间
    /// 改等待分钟数 / 漂移重应用」的多重 begin 场景。
    #[test]
    #[ignore = "真实修改电源设置，仅在目标机器上手动验证"]
    fn real_backup_not_overwritten_by_reapply() {
        let _g = real_power_lock().lock().unwrap_or_else(|e| e.into_inner());
        let original = read_display_timeouts(VIDEOIDLE).expect("应能读到当前关屏超时");
        begin_display_off(300).expect("begin(300) 应成功（备份原值并写入 300）");
        begin_display_off(600).expect("begin(600) 应成功（只更新值，不覆盖备份）");
        end_display_off();
        assert_eq!(
            read_display_timeouts(VIDEOIDLE).unwrap(),
            original,
            "end() 应恢复最初原值——备份未被二次 begin 覆盖"
        );
    }
}

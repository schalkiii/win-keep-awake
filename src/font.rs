//! 中文字体支持：egui 自带字体（Hack + Noto Emoji）不含 CJK 字形，
//! 直接渲染中文会出现"豆腐块"。这里在运行时加载 Windows 系统自带的中文字体，
//! 以"字形回退（fallback）"方式注入 —— 拉丁字符仍由原字体绘制，仅中文由系统中文字体补齐，
//! 保证与系统其他软件一致的观感，且不引入任何外部字体文件（独立 exe 诉求不受影响）。

use std::path::PathBuf;

use eframe::egui::{FontData, FontFamily};
use eframe::egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};

/// 按优先级排列的候选字体（Windows 自带）。
/// 用 `ttc` 时取第 0 个 face（即"微软雅黑"常规字重），skrifa 会自动处理集合索引。
const CANDIDATES: &[&str] = &[
    r"Microsoft YaHei UI Regular\msyhui.ttc",
    r"Microsoft YaHei\msyh.ttc",
    "msyh.ttf",
    "msyhbd.ttf",
    "simhei.ttf",
    "simsun.ttc",
    "simkai.ttf",
];

fn fonts_dir() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("Fonts")
}

/// 找到一个可用的中文字体文件并读取其字节。找不到时返回 `None`（GUI 仍可运行，只是中文显示为方块）。
fn load_cjk_font() -> Option<(String, Vec<u8>)> {
    let dir = fonts_dir();
    for candidate in CANDIDATES {
        let path = dir.join(candidate);
        if let Ok(bytes) = std::fs::read(&path) {
            if !bytes.is_empty() {
                return Some((path.display().to_string(), bytes));
            }
        }
    }
    None
}

/// 将中文字体注册为 egui 全局字体的字形回退。在 `apply_theme` 之后调用。
/// 返回实际使用的字体路径（供日志展示）。
pub fn install_cjk_font(ctx: &eframe::egui::Context) -> Option<String> {
    let (path, bytes) = load_cjk_font()?;
    let insert = FontInsert::new(
        "system_cjk",
        FontData::from_owned(bytes),
        vec![
            InsertFontFamily {
                family: FontFamily::Proportional,
                priority: FontPriority::Lowest,
            },
            InsertFontFamily {
                family: FontFamily::Monospace,
                priority: FontPriority::Lowest,
            },
        ],
    );
    ctx.add_font(insert);
    Some(path)
}

// ------------------------------------------------------------------ 测试 ----

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_font_lookup_is_environment_tolerant() {
        // 目标机（Windows 桌面）通常自带微软雅黑/黑体/宋体；但 CI 的 Windows Server
        // 镜像可能未安装 CJK 字体。故：找不到时不失败（GUI 会降级为方块），
        // 找到则校验它是合理的字体文件。
        match load_cjk_font() {
            Some((path, bytes)) => {
                assert!(!path.is_empty());
                assert!(bytes.len() > 1024, "字体文件异常过小: {} B", bytes.len());
            }
            None => {
                eprintln!("skip: 本环境未找到系统 CJK 字体（常见于精简的 CI 镜像），非缺陷");
            }
        }
    }
}

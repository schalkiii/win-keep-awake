//! 构建期资源生成：
//! 1. 复用 `src/icon.rs` 的绘制代码（include! 进来，保证 exe 图标与托盘/窗口图标同一设计），
//!    渲染 16–256px 七档 RGBA，PNG 编码后组装成 .ico；
//! 2. 通过 winresource 把图标与 VERSIONINFO 嵌入 exe 的 PE 资源段。
//!
//! "独立 exe" 硬约束不受影响：没有任何外部运行时文件——图标编译进了 exe 本体。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

// 与运行时同一份绘制代码（作为真实模块文件引入，//! 文档注释合法；
// cfg(test) 的测试在构建脚本里自动排除）。allow 抑制"仅运行时使用"条目的 dead_code 噪音。
#[allow(dead_code)]
#[path = "src/icon.rs"]
mod icon;

const ICON_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

fn png_encode(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, size, size);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().expect("PNG header");
        writer.write_image_data(rgba).expect("PNG data");
    }
    out
}

/// 组装多尺寸 PNG-entries ICO（Vista+ 均支持 PNG 压缩条目）。
fn build_ico(dir: &Path) -> PathBuf {
    let mut entries: Vec<(u32, Vec<u8>)> = Vec::new();
    for &s in &ICON_SIZES {
        let rgba = icon::build_mode_icon_rgba(s, icon::IconVariant::Awake);
        entries.push((s, png_encode(s, &rgba)));
    }

    let mut ico: Vec<u8> = Vec::new();
    ico.extend_from_slice(&0u16.to_le_bytes()); // reserved
    ico.extend_from_slice(&1u16.to_le_bytes()); // type = icon
    ico.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * entries.len() as u32;
    for &(s, ref data) in &entries {
        let (wb, hb) = if s >= 256 {
            (0u8, 0u8)
        } else {
            (s as u8, s as u8)
        };
        ico.extend_from_slice(&[wb, hb, 0, 0]); // 宽、高、调色板数、保留
        ico.extend_from_slice(&1u16.to_le_bytes()); // planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bpp
        ico.extend_from_slice(&(data.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in &entries {
        ico.extend_from_slice(data);
    }

    let path = dir.join("WinKeepAwake.ico");
    fs::write(&path, ico).expect("写入 .ico 失败");
    path
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon.rs");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let ico_path = build_ico(&out_dir);

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let version = env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let mut res = winresource::WindowsResource::new();
    res.set_icon(&ico_path.display().to_string());
    res.set(
        "FileDescription",
        "WinKeepAwake - keep-awake tray tool (screen-on / screen-off-but-awake)",
    );
    res.set("ProductName", "WinKeepAwake");
    res.set("ProductVersion", &version);
    res.set("FileVersion", &version);
    res.set("OriginalFilename", "WinKeepAwake.exe");
    res.set("InternalName", "WinKeepAwake");
    res.set("LegalCopyright", "MIT License");
    if let Err(e) = res.compile() {
        panic!("嵌入 Windows 资源失败（需要 Windows SDK 的 rc.exe）: {e}");
    }
}

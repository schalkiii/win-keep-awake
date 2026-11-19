//! 程序化生成的应用图标（无外部图片文件依赖，保证"独立 exe"诉求）。
//! 同一份绘制代码有两处消费：
//! 1. 运行时：托盘图标（随工作模式切换）+ 窗口/任务栏图标；
//! 2. 构建期：build.rs 通过 `include!` 复用本文件，渲染多尺寸 .ico 嵌入 exe 资源段。
//!
//! 两个设计（品牌一致、语义区分）：
//! - `Awake`（咖啡杯，青色徽章）：防息屏模式 —— "提神、保持清醒"。
//! - `NightGuard`（月牙 + 星，靛蓝徽章）：息屏防睡模式 —— "夜里守着，屏幕暗了系统还醒着"。
//!
//! 绘制采用带符号距离场（SDF）+ smoothstep 抗锯齿 + 2× 超采样，
//! 保证 16px 托盘尺寸到 256px exe 图标都干净利落。
//! （UI 主题强调色已移至 main.rs 的调色板常量，与本文件解耦。）

/// 图标变体：防息屏（咖啡） / 息屏防睡（月牙）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconVariant {
    /// 咖啡杯 · 青色徽章 —— 防息屏（屏幕常亮）。
    Awake,
    /// 月牙 + 星 · 靛蓝徽章 —— 息屏防睡（关屏不休眠）。
    NightGuard,
}

/// 品牌图标（咖啡 / 防息屏）。窗口图标与 exe 嵌入图标用它，保持与历史调用方兼容。
pub fn build_icon_rgba(size: u32) -> Vec<u8> {
    build_mode_icon_rgba(size, IconVariant::Awake)
}

/// 按变体渲染图标：RGBA 行主序、非预乘、尺寸 `size x size`。
pub fn build_mode_icon_rgba(size: u32, variant: IconVariant) -> Vec<u8> {
    let size = size.max(1);
    let ss = 2u32; // 超采样倍数
    let full = size * ss;
    let mut hi = vec![0u8; (full * full * 4) as usize];

    let badge = variant.badge_colors();
    for y in 0..full {
        for x in 0..full {
            // 像素中心映射到 0..1 单位坐标（SDF 在单位空间求值，aa 以超采样像素计）
            let px = (x as f32 + 0.5) / full as f32;
            let py = (y as f32 + 0.5) / full as f32;
            let aa = 1.0 / full as f32;

            // 1) 圆角方形徽章（垂直渐变），四角透明
            let sd_badge = sd_rounded_box(px, py, 0.5, 0.5, 0.5, 0.5, 0.21);
            let cov_badge = coverage(sd_badge, aa);
            if cov_badge > 0.0 {
                let t = py; // 上亮下深
                let (r, g, b) = (
                    lerp_f(badge.0[0], badge.1[0], t),
                    lerp_f(badge.0[1], badge.1[1], t),
                    lerp_f(badge.0[2], badge.1[2], t),
                );
                blend_px(&mut hi, full, x, y, (r, g, b, (cov_badge * 255.0) as u8));
            }

            // 2) 前景元素按变体绘制
            for shape in variant.shapes() {
                let sd = (shape.sdf)(px, py);
                let cov = coverage(sd, aa) * shape.alpha;
                if cov > 0.0 {
                    blend_px(
                        &mut hi,
                        full,
                        x,
                        y,
                        (
                            shape.rgb[0],
                            shape.rgb[1],
                            shape.rgb[2],
                            (cov * 255.0) as u8,
                        ),
                    );
                }
            }
        }
    }

    // 3) 2x2 盒式下采样（按预乘 alpha 平均，边缘更干净）
    downsample(&hi, full, ss)
}

impl IconVariant {
    /// 徽章渐变的（顶部色, 底部色）。
    fn badge_colors(self) -> ([u8; 3], [u8; 3]) {
        match self {
            // 青 → 深青（防息屏，与 UI 强调色同族）
            IconVariant::Awake => ([56, 199, 255], [8, 105, 158]),
            // 靛蓝 → 深靛蓝（息屏防睡，"夜"的色系，托盘里一眼可辨）
            IconVariant::NightGuard => ([99, 102, 241], [37, 33, 122]),
        }
    }

    /// 前景形状列表（依次叠加）。
    fn shapes(self) -> &'static [Shape] {
        match self {
            IconVariant::Awake => AWAKE_SHAPES,
            IconVariant::NightGuard => NIGHT_SHAPES,
        }
    }
}

/// 一个前景形状：SDF + 颜色 + 不透明度。
struct Shape {
    sdf: fn(f32, f32) -> f32,
    rgb: [u8; 3],
    alpha: f32,
}

const WHITE: [u8; 3] = [248, 250, 252];
const AMBER: [u8; 3] = [252, 211, 77];

/// 咖啡杯：两缕热气 + 杯身 + 杯柄 + 托碟。
static AWAKE_SHAPES: &[Shape] = &[
    // 热气 1（竖向胶囊，微弯不做——小尺寸会糊，挺拔更清晰）
    Shape {
        sdf: |x, y| sd_capsule_v(x, y, 0.415, 0.155, 0.325, 0.030),
        rgb: WHITE,
        alpha: 0.92,
    },
    // 热气 2
    Shape {
        sdf: |x, y| sd_capsule_v(x, y, 0.545, 0.185, 0.345, 0.030),
        rgb: WHITE,
        alpha: 0.80,
    },
    // 杯身（圆角矩形，上宽下略窄用两个半径近似——统一半径即可）
    Shape {
        sdf: |x, y| sd_rounded_box(x, y, 0.415, 0.565, 0.145, 0.135, 0.055),
        rgb: WHITE,
        alpha: 1.0,
    },
    // 杯柄（圆环，与杯身相交）
    Shape {
        sdf: |x, y| sd_ring(x, y, 0.640, 0.545, 0.100, 0.033),
        rgb: WHITE,
        alpha: 1.0,
    },
    // 托碟
    Shape {
        sdf: |x, y| sd_rounded_box(x, y, 0.485, 0.795, 0.215, 0.030, 0.030),
        rgb: WHITE,
        alpha: 0.95,
    },
];

/// 月牙 + 两颗星。
static NIGHT_SHAPES: &[Shape] = &[
    // 月牙 = 大圆减去右上偏移圆
    Shape {
        sdf: |x, y| {
            let d1 = sd_circle(x, y, 0.44, 0.46, 0.295);
            let d2 = sd_circle(x, y, 0.585, 0.355, 0.265);
            d1.max(-d2)
        },
        rgb: AMBER,
        alpha: 1.0,
    },
    // 四芒星（十字双胶囊）在月牙开口处
    Shape {
        sdf: |x, y| sd_star(x, y, 0.695, 0.660, 0.085, 0.017),
        rgb: WHITE,
        alpha: 1.0,
    },
    // 小星点
    Shape {
        sdf: |x, y| sd_circle(x, y, 0.300, 0.735, 0.026),
        rgb: WHITE,
        alpha: 0.95,
    },
];

// ------------------------------------------------------------------ SDF 工具 ----

fn sd_circle(px: f32, py: f32, cx: f32, cy: f32, r: f32) -> f32 {
    ((px - cx) * (px - cx) + (py - cy) * (py - cy)).sqrt() - r
}

fn sd_rounded_box(px: f32, py: f32, cx: f32, cy: f32, hx: f32, hy: f32, r: f32) -> f32 {
    let qx = (px - cx).abs() - (hx - r);
    let qy = (py - cy).abs() - (hy - r);
    let ax = qx.max(0.0);
    let ay = qy.max(0.0);
    ax.hypot(ay) + qx.max(qy).min(0.0) - r
}

fn sd_ring(px: f32, py: f32, cx: f32, cy: f32, r: f32, half_thick: f32) -> f32 {
    sd_circle(px, py, cx, cy, r).abs() - half_thick
}

fn sd_capsule_v(px: f32, py: f32, x: f32, y0: f32, y1: f32, half_w: f32) -> f32 {
    let cy = py.clamp(y0, y1);
    (px - x).hypot(py - cy) - half_w
}

/// 四芒星 = 横竖两条胶囊的并集（SDF 取 min）。
fn sd_star(px: f32, py: f32, cx: f32, cy: f32, half_len: f32, half_w: f32) -> f32 {
    let dx = (px - cx).abs();
    let dy = (py - cy).abs();
    // 横向胶囊（沿 x 轴）：q = (dx-half_len, dy) 的标准体育场 SDF
    let qx = dx - half_len;
    let h = qx.max(0.0).hypot(dy) + qx.max(dy).min(0.0) - half_w;
    // 竖向胶囊（沿 y 轴）
    let qy = dy - half_len;
    let v = qy.max(0.0).hypot(dx) + qy.max(dx).min(0.0) - half_w;
    h.min(v)
}

/// SDF → 覆盖率（1px 平滑带抗锯齿）。
fn coverage(sd: f32, aa: f32) -> f32 {
    1.0 - smoothstep(-aa, aa, sd)
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn lerp_f(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 * (1.0 - t) + b as f32 * t)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// 简单 src-over 合成（源为纯色 + 给定 alpha，目标是当前像素）。
fn blend_px(buf: &mut [u8], size: u32, x: u32, y: u32, rgba: (u8, u8, u8, u8)) {
    let (r, g, b, a) = rgba;
    let i = ((y * size + x) * 4) as usize;
    let dst_a = buf[i + 3] as f32 / 255.0;
    let src_a = a as f32 / 255.0;
    let out_a = src_a + dst_a * (1.0 - src_a);
    if out_a <= 0.0 {
        return;
    }
    let blend = |s: u8, d: u8| {
        let s = s as f32;
        let d = d as f32;
        ((s * src_a + d * dst_a * (1.0 - src_a)) / out_a)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    buf[i] = blend(r, buf[i]);
    buf[i + 1] = blend(g, buf[i + 1]);
    buf[i + 2] = blend(b, buf[i + 2]);
    buf[i + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

/// ss×ss 盒式下采样（预乘 alpha 平均）。
fn downsample(hi: &[u8], full: u32, ss: u32) -> Vec<u8> {
    let size = full / ss;
    let n = (ss * ss) as f32;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (mut ar, mut ag, mut ab, mut aa) = (0f32, 0f32, 0f32, 0f32);
            for dy in 0..ss {
                for dx in 0..ss {
                    let sx = x * ss + dx;
                    let sy = y * ss + dy;
                    let i = ((sy * full + sx) * 4) as usize;
                    let a = hi[i + 3] as f32 / 255.0;
                    ar += hi[i] as f32 * a;
                    ag += hi[i + 1] as f32 * a;
                    ab += hi[i + 2] as f32 * a;
                    aa += a;
                }
            }
            let o = ((y * size + x) * 4) as usize;
            if aa > 0.0 {
                out[o] = (ar / aa).round().clamp(0.0, 255.0) as u8;
                out[o + 1] = (ag / aa).round().clamp(0.0, 255.0) as u8;
                out[o + 2] = (ab / aa).round().clamp(0.0, 255.0) as u8;
            }
            out[o + 3] = ((aa / n) * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

// ------------------------------------------------------------------ 测试 ----

#[cfg(test)]
mod tests {
    use super::*;

    fn px(buf: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn buffer_size_matches_dimensions() {
        for size in [16u32, 32, 64, 256] {
            for variant in [IconVariant::Awake, IconVariant::NightGuard] {
                let buf = build_mode_icon_rgba(size, variant);
                assert_eq!(buf.len(), (size * size * 4) as usize);
            }
        }
    }

    #[test]
    fn awake_center_is_white_cup_and_corner_transparent() {
        let size = 64;
        let buf = build_mode_icon_rgba(size, IconVariant::Awake);
        // 中心落在杯身上：应为近白且不透明
        let center = px(&buf, size, size / 2, size / 2);
        assert_eq!(center[3], 255, "中心点应完全不透明");
        assert!(
            center[0] > 200 && center[1] > 200 && center[2] > 200,
            "中心应为白色杯身: {center:?}"
        );
        let corner = px(&buf, size, 0, 0);
        assert_eq!(corner[3], 0, "徽章圆角外应完全透明");
    }

    #[test]
    fn awake_badge_is_cyan_family() {
        let size = 64;
        let buf = build_mode_icon_rgba(size, IconVariant::Awake);
        // 徽章左缘中点（无前景处）：蓝色分量应明显高于红色（青色系）
        let edge = px(&buf, size, 1, size / 2);
        assert!(edge[3] > 0, "徽章边缘应不透明: {edge:?}");
        assert!(edge[2] > edge[0], "应为青色系: {edge:?}");
    }

    #[test]
    fn night_guard_has_amber_moon_on_indigo_badge() {
        let size = 64;
        let buf = build_mode_icon_rgba(size, IconVariant::NightGuard);
        // 月牙可见部分（大圆内、挖除圆外）：暖色（R > B）
        let moon = px(
            &buf,
            size,
            (0.32 * size as f32) as u32,
            (0.60 * size as f32) as u32,
        );
        assert!(moon[3] == 255, "月牙应不透明: {moon:?}");
        assert!(moon[0] > moon[2], "月牙应为暖色（琥珀）: {moon:?}");
        // 挖除区域中心：靛蓝徽章（B > R）
        let bite = px(&buf, size, size / 2, size / 2);
        assert!(bite[2] > bite[0], "月牙挖除处应露出靛蓝徽章: {bite:?}");
        let corner = px(&buf, size, 0, 0);
        assert_eq!(corner[3], 0, "徽章圆角外应完全透明");
    }

    #[test]
    fn variants_are_visually_distinct() {
        let size = 32;
        let a = build_mode_icon_rgba(size, IconVariant::Awake);
        let n = build_mode_icon_rgba(size, IconVariant::NightGuard);
        assert_ne!(a, n, "两个变体渲染结果应有区别");
    }
}

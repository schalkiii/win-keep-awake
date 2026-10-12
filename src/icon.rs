//! 程序化生成的应用图标（无外部图片文件依赖，保证"独立 exe"诉求）。
//! 用于系统托盘图标 + 窗口/任务栏图标，共用同一份 RGBA 数据。

/// 强调色（与 UI 主题保持一致）：亮青色
pub const ACCENT: [u8; 3] = [56, 189, 248];
/// 强调色（深色端，用于径向渐变的外圈）
pub const ACCENT_DARK: [u8; 3] = [8, 89, 121];

/// 生成 `size x size` 的 RGBA 图标：
/// 背景是一个带径向渐变 + 抗锯齿边缘的圆角圆盘，中间是一个白色圆点，
/// 左右各一条短横"运动线"，示意"轻微抖动但不移动"这一核心概念。
pub fn build_icon_rgba(size: u32) -> Vec<u8> {
    let mut buf = vec![0u8; (size * size * 4) as usize];
    let c = size as f32 / 2.0;
    let r_disk = c - 1.5;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - c;
            let dy = y as f32 + 0.5 - c;
            let dist = (dx * dx + dy * dy).sqrt();

            // 圆盘本体：抗锯齿边缘用 1px 宽度的 smoothstep 过渡
            let disk_alpha = smoothstep(r_disk + 0.5, r_disk - 0.5, dist);
            if disk_alpha > 0.0 {
                let t = (dist / r_disk).clamp(0.0, 1.0);
                let cr = lerp_u8(ACCENT[0], ACCENT_DARK[0], t);
                let cg = lerp_u8(ACCENT[1], ACCENT_DARK[1], t);
                let cb = lerp_u8(ACCENT[2], ACCENT_DARK[2], t);
                blend_px(&mut buf, size, x, y, (cr, cg, cb, (disk_alpha * 255.0) as u8));
            }

            // 中心圆点
            let dot_r = size as f32 * 0.11;
            let dot_alpha = smoothstep(dot_r + 0.5, dot_r - 0.5, dist);
            if dot_alpha > 0.0 {
                blend_px(&mut buf, size, x, y, (255, 255, 255, (dot_alpha * 255.0) as u8));
            }

            // 左右两条短运动线（白色，半透明，示意"抖动"方向）
            let line_half_h = size as f32 * 0.045;
            let line_len = size as f32 * 0.17;
            let gap = size as f32 * 0.26;
            for &side in &[-1.0f32, 1.0] {
                let cx_line = side * (dot_r + gap);
                let lx = (dx - cx_line).abs();
                let ly = dy.abs();
                if lx <= line_len / 2.0 && ly <= line_half_h {
                    let fade = 1.0 - (ly / line_half_h).clamp(0.0, 1.0);
                    let a = (fade * 0.85 * 255.0) as u8;
                    blend_px(&mut buf, size, x, y, (255, 255, 255, a));
                }
            }
        }
    }
    buf
}

/// edge0 > edge1 时表示"距离越近 alpha 越高"的反向 smoothstep（用于软边圆形）。
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 * (1.0 - t) + b as f32 * t).round().clamp(0.0, 255.0) as u8
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
        ((s * src_a + d * dst_a * (1.0 - src_a)) / out_a).round().clamp(0.0, 255.0) as u8
    };
    buf[i] = blend(r, buf[i]);
    buf[i + 1] = blend(g, buf[i + 1]);
    buf[i + 2] = blend(b, buf[i + 2]);
    buf[i + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
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
        for size in [16u32, 32, 64] {
            let buf = build_icon_rgba(size);
            assert_eq!(buf.len(), (size * size * 4) as usize);
        }
    }

    #[test]
    fn center_is_opaque_and_corner_is_transparent() {
        let size = 32;
        let buf = build_icon_rgba(size);
        let center = px(&buf, size, size / 2, size / 2);
        assert_eq!(center[3], 255, "中心点应完全不透明");
        // 中心白点：各通道接近白
        assert!(center[0] > 200 && center[1] > 200 && center[2] > 200, "中心应为白色: {center:?}");
        let corner = px(&buf, size, 0, 0);
        assert_eq!(corner[3], 0, "圆外四角应完全透明");
    }

    #[test]
    fn disk_edge_carries_accent_tone() {
        let size = 32;
        let buf = build_icon_rgba(size);
        // 圆盘外边缘附近（靠近渐变深色端）应带强调色蓝/青分量且非全透明
        let edge = px(&buf, size, 2, size / 2);
        assert!(edge[3] > 0, "圆盘边缘应不透明: {edge:?}");
        assert!(edge[2] > edge[0], "蓝色分量应高于红色（青色系）: {edge:?}");
    }
}

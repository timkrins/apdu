use ratatui::layout::Rect;

#[derive(Clone, Copy)]
struct Area {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn worst(row: &[f64], side: f64) -> f64 {
    let sum: f64 = row.iter().sum();
    let (max, min) = row.iter().fold((0f64, f64::MAX), |(a, b), &v| (a.max(v), b.min(v)));
    let s2 = side * side;
    (s2 * max / (sum * sum)).max(sum * sum / (s2 * min))
}

fn squarify(values: &[f64], mut r: Area) -> Vec<Area> {
    let mut out = Vec::with_capacity(values.len());
    let mut i = 0;
    while i < values.len() {
        let side = r.w.min(r.h);
        let mut end = i + 1;
        while end < values.len() && worst(&values[i..=end], side) <= worst(&values[i..end], side) {
            end += 1;
        }
        let row = &values[i..end];
        let sum: f64 = row.iter().sum();
        if r.w >= r.h {
            let w = sum / r.h;
            let mut y = r.y;
            for v in row {
                out.push(Area { x: r.x, y, w, h: v / w });
                y += v / w;
            }
            r.x += w;
            r.w -= w;
        } else {
            let h = sum / r.w;
            let mut x = r.x;
            for v in row {
                out.push(Area { x, y: r.y, w: v / h, h });
                x += v / h;
            }
            r.y += h;
            r.h -= h;
        }
        i = end;
    }
    out
}

/// Lays out `sizes` (sorted biggest first) in `area`, returning one cell rect per size;
/// sizes too small to get a cell come back empty. Terminal cells are about twice as tall
/// as wide, so the layout runs in half-cell rows to keep tiles square on screen.
pub fn layout(sizes: &[u64], area: Rect) -> Vec<Rect> {
    let total: u64 = sizes.iter().sum();
    if total == 0 || area.width == 0 || area.height == 0 {
        return vec![Rect::default(); sizes.len()];
    }
    let (w, h) = (area.width as f64, area.height as f64 * 2.0);
    let scale = w * h / total as f64;
    let values: Vec<f64> = sizes.iter().map(|&s| (s as f64 * scale).max(1e-9)).collect();
    squarify(&values, Area { x: 0.0, y: 0.0, w, h })
        .into_iter()
        .map(|a| {
            let (x0, x1) = (a.x.round() as u16, (a.x + a.w).round() as u16);
            let (y0, y1) = ((a.y / 2.0).round() as u16, ((a.y + a.h) / 2.0).round() as u16);
            Rect {
                x: area.x + x0.min(area.width),
                y: area.y + y0.min(area.height),
                width: x1.min(area.width).saturating_sub(x0),
                height: y1.min(area.height).saturating_sub(y0),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_cover_the_area_without_overlap() {
        let area = Rect::new(0, 0, 80, 24);
        let rects = layout(&[500, 300, 100, 60, 30, 10], area);
        let mut cells = vec![0u8; 80 * 24];
        for r in &rects {
            for y in r.y..r.y + r.height {
                for x in r.x..r.x + r.width {
                    cells[y as usize * 80 + x as usize] += 1;
                }
            }
        }
        assert!(cells.iter().all(|&c| c == 1));
    }

    #[test]
    fn tile_area_tracks_size() {
        let rects = layout(&[3, 1], Rect::new(0, 0, 40, 20));
        let a = |r: &Rect| r.width as u32 * r.height as u32;
        assert_eq!(a(&rects[0]) + a(&rects[1]), 800);
        assert!((a(&rects[0]) as f64 / 800.0 - 0.75).abs() < 0.05);
    }
}

use std::time::Duration;

const MB: f64 = 1024.0 * 1024.0;
const GB: f64 = MB * 1024.0;

/// "612 MB", "1.21 GB".
pub fn bytes(value: u64) -> String {
    let v = value as f64;
    if v >= GB {
        format!("{:.2} GB", v / GB)
    } else if v >= MB {
        format!("{:.0} MB", v / MB)
    } else {
        format!("{:.0} KB", v / 1024.0)
    }
}

/// A headline total split into number and unit: ("4.8", "GB").
pub fn total(value: u64) -> (String, &'static str) {
    let v = value as f64;
    if v >= GB { (format!("{:.1}", v / GB), "GB") } else { (format!("{:.0}", v / MB), "MB") }
}

/// "40s", "12m", "3h", "2d".
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

pub fn percent(value: f64) -> String {
    if value < 10.0 { format!("{value:.1}%") } else { format!("{value:.0}%") }
}

pub fn tilde(path: &str) -> String {
    let home = crate::home();
    match path.strip_prefix(home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// An eight-level sparkline over the last `width` values, scaled to their
/// range (at least a quarter of the peak, so a 1 MB wobble stays low); or,
/// with `ceiling`, from zero to at least that value, so a CPU going from 0%
/// to 0.1% stays flat.
pub fn sparkline(values: &[f64], width: usize, ceiling: Option<f64>) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let tail = &values[values.len().saturating_sub(width)..];
    let (mut min, mut max) = tail.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    if let Some(ceiling) = ceiling {
        (min, max) = (0.0, max.max(ceiling));
    }
    let range = if ceiling.is_some() { max - min } else { (max - min).max(max.abs() * 0.25) };
    let line: String = tail
        .iter()
        .map(|&v| {
            // Flat lines sit low; only real movement climbs.
            if range <= max.abs() * 0.01 { BARS[0] } else { BARS[(((v - min) / range) * 7.0).round() as usize] }
        })
        .collect();
    format!("{}{line}", " ".repeat(width - tail.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes_and_times() {
        assert_eq!(bytes(612 * 1024 * 1024), "612 MB");
        assert_eq!(bytes((1.21 * GB) as u64), "1.21 GB");
        assert_eq!(total((4.8 * GB) as u64), ("4.8".into(), "GB"));
        assert_eq!(duration(Duration::from_secs(3 * 3600 + 5)), "3h");
        assert_eq!(duration(Duration::from_secs(45)), "45s");
    }

    #[test]
    fn sparkline_pads_and_scales() {
        assert_eq!(sparkline(&[1.0, 2.0], 4, None), "  ▁█");
        assert_eq!(sparkline(&[5.0, 5.0, 5.0], 3, None), "▁▁▁");
        assert_eq!(sparkline(&[100.0, 101.0], 2, None), "▁▁");
        assert_eq!(sparkline(&[0.0, 0.1], 2, Some(10.0)), "▁▁");
        assert_eq!(sparkline(&[0.0, 10.0], 2, Some(10.0)), "▁█");
    }
}

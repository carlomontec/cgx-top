//! Terminal sparkline generator using Unicode block characters (U+2581 to U+2588).

const SPARK_CHARS: [char; 8] = [' ', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Render a series of float values into a single-line terminal sparkline string.
pub fn render_sparkline(values: &[f32]) -> String {
    if values.is_empty() {
        return String::new();
    }

    let mut min = f32::INFINITY;
    let mut max = -f32::INFINITY;

    for &v in values {
        if v.is_finite() {
            if v < min { min = v; }
            if v > max { max = v; }
        }
    }

    if min > max {
        return " ".repeat(values.len());
    }

    let range = max - min;
    let mut s = String::with_capacity(values.len());

    for &v in values {
        if !v.is_finite() {
            s.push(' ');
        } else if range < 1e-12 {
            s.push('▄'); // Mid-height flat line
        } else {
            let ratio = ((v - min) / range).clamp(0.0, 1.0);
            let idx = (ratio * 7.0).round() as usize;
            s.push(SPARK_CHARS[idx.min(7)]);
        }
    }

    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sparkline() {
        let vals = vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0];
        assert_eq!(render_sparkline(&vals), " ▂▃▄▅▆▇█");

        let flat = vec![5.0, 5.0, 5.0];
        assert_eq!(render_sparkline(&flat), "▄▄▄");
    }
}

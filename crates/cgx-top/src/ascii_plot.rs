//! High-resolution Braille / ASCII plot renderer for CalculiX time histories.

/// Renders a series of scalar values into a vector of strings suitable for printing
/// within a terminal box or directly to stdout.
///
/// * `values`: The sequence of data points (e.g. peak stress or displacement per step).
/// * `canvas_cols`: Number of character columns for the plot area (e.g. 58).
/// * `canvas_rows`: Number of character rows for the plot area (e.g. 7).
/// * `x_label_left`: Optional label for the start of the x-axis (e.g. "Step 1").
/// * `x_label_right`: Optional label for the end of the x-axis (e.g. "Step 200").
pub fn render_ascii_plot(
    values: &[f32],
    canvas_cols: usize,
    canvas_rows: usize,
    x_label_left: Option<&str>,
    x_label_right: Option<&str>,
) -> Vec<String> {
    if values.is_empty() || canvas_cols == 0 || canvas_rows == 0 {
        return Vec::new();
    }

    let min_val = values.iter().copied().fold(f32::INFINITY, f32::min);
    let max_val = values.iter().copied().fold(-f32::INFINITY, f32::max);
    let range = if (max_val - min_val).abs() < 1e-12 {
        1.0
    } else {
        max_val - min_val
    };

    let n = values.len();

    // Map each column to a row coordinate using linear interpolation between data points
    let mut col_y: Vec<usize> = Vec::with_capacity(canvas_cols);
    for c in 0..canvas_cols {
        let t = if canvas_cols <= 1 {
            0.0
        } else {
            (c as f64 / (canvas_cols - 1) as f64) * (n - 1) as f64
        };
        let idx = (t.floor() as usize).min(n - 1);
        let next_idx = (idx + 1).min(n - 1);
        let alpha = (t - idx as f64) as f32;
        let v = (1.0 - alpha) * values[idx] + alpha * values[next_idx];
        let norm = ((v - min_val) / range).clamp(0.0, 1.0);
        let r = ((1.0 - norm) * (canvas_rows - 1) as f32).round() as usize;
        col_y.push(r.min(canvas_rows - 1));
    }

    // Ports bitmask: NORTH = 1, EAST = 2, SOUTH = 4, WEST = 8
    const NORTH: u8 = 1;
    const EAST: u8 = 2;
    const SOUTH: u8 = 4;
    const WEST: u8 = 8;

    let mut ports = vec![vec![0u8; canvas_cols]; canvas_rows];

    for c in 0..(canvas_cols - 1) {
        let y1 = col_y[c];
        let y2 = col_y[c + 1];

        if y1 == y2 {
            ports[y1][c] |= EAST;
            ports[y2][c + 1] |= WEST;
        } else if y2 < y1 {
            // Going up
            ports[y1][c] |= EAST;
            ports[y1][c + 1] |= WEST | NORTH;
            for r in (y2 + 1)..y1 {
                ports[r][c + 1] |= NORTH | SOUTH;
            }
            ports[y2][c + 1] |= SOUTH;
        } else {
            // Going down
            ports[y1][c] |= EAST;
            ports[y1][c + 1] |= WEST | SOUTH;
            for r in (y1 + 1)..y2 {
                ports[r][c + 1] |= NORTH | SOUTH;
            }
            ports[y2][c + 1] |= NORTH;
        }
    }

    let mut lines: Vec<String> = Vec::new();
    let y_axis_width = 10;

    for r in 0..canvas_rows {
        let mut row_chars = String::with_capacity(canvas_cols);
        for c in 0..canvas_cols {
            let p = ports[r][c];
            let ch = match p {
                0 => ' ',
                // Horizontal
                p if p == EAST || p == WEST || p == (EAST | WEST) => '─',
                // Vertical
                p if p == NORTH || p == SOUTH || p == (NORTH | SOUTH) => '│',
                // Rounded corners
                p if p == (SOUTH | EAST) => '╭',
                p if p == (SOUTH | WEST) => '╮',
                p if p == (NORTH | EAST) => '╰',
                p if p == (NORTH | WEST) => '╯',
                // T-junctions
                p if p == (NORTH | SOUTH | EAST) => '├',
                p if p == (NORTH | SOUTH | WEST) => '┤',
                p if p == (NORTH | EAST | WEST) => '┴',
                p if p == (SOUTH | EAST | WEST) => '┬',
                p if p == (NORTH | SOUTH | EAST | WEST) => '┼',
                _ => '─',
            };
            row_chars.push(ch);
        }

        // Generate Y-axis tick on the left
        let tick_label = if r == 0 {
            format!("{:>width$.2e} ┤ ", max_val, width = y_axis_width)
        } else if r == canvas_rows / 2 {
            let mid_val = min_val + range * 0.5;
            format!("{:>width$.2e} ┤ ", mid_val, width = y_axis_width)
        } else if r == canvas_rows - 1 {
            format!("{:>width$.2e} ┤ ", min_val, width = y_axis_width)
        } else {
            format!("{:>width$} │ ", "", width = y_axis_width)
        };

        lines.push(format!("{}{}", tick_label, row_chars));
    }

    // X-axis border line: "           └───...───"
    let axis_line = format!("{:>width$} └──{}", "", "─".repeat(canvas_cols.saturating_sub(1)), width = y_axis_width);
    lines.push(axis_line);

    // X-axis labels
    let left = x_label_left.unwrap_or("");
    let right = x_label_right.unwrap_or("");
    let left_pad = y_axis_width + 3;
    let total_axis_width = canvas_cols;
    let label_line = if total_axis_width > left.len() + right.len() + 2 {
        let space_between = total_axis_width - left.len() - right.len();
        format!("{:>left_pad$}{}{}{}", "", left, " ".repeat(space_between), right)
    } else {
        format!("{:>left_pad$}{}", "", left)
    };
    lines.push(label_line);

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_values() {
        let result = render_ascii_plot(&[], 40, 5, None, None);
        assert!(result.is_empty());
    }

    #[test]
    fn test_flat_values() {
        let values = vec![5.0; 10];
        let result = render_ascii_plot(&values, 40, 5, Some("0"), Some("10"));
        assert_eq!(result.len(), 7); // 5 rows + 1 axis + 1 label
    }

    #[test]
    fn test_sine_curve_dimensions() {
        let values: Vec<f32> = (0..20)
            .map(|i| (i as f32 * 0.314).sin() * 100.0)
            .collect();
        let result = render_ascii_plot(&values, 50, 6, Some("Start"), Some("End"));
        assert_eq!(result.len(), 8); // 6 rows + axis line + label line
        for (i, line) in result.iter().enumerate() {
            assert!(line.len() > 0, "Line {} should not be empty", i);
        }
    }

    #[test]
    fn test_plot_line_lengths() {
        let values = vec![84.5, 80.6, 85.9, 81.4, 28.2, 86.1, 80.1, 26.9, 165.9, 166.9, 85.2, 74.5];
        let lines = render_ascii_plot(&values, 56, 7, Some("Step 1"), Some("Step 12"));
        for line in &lines {
            assert_eq!(line.chars().count(), 69, "Every plot line must be exactly 69 characters");
        }
    }
}

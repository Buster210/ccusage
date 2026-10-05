use std::io::{self, Write};

use crate::{
    style::{Color, TerminalStyle, bold, color},
    terminal::DEFAULT_TERMINAL_WIDTH,
    width::{
        ansi_continuation, ensure_ansi_reset, truncate_to_width, visible_width,
        visible_width_max_line,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

struct TableRow {
    cells: Vec<String>,
    cell_alignment: Option<(usize, Align)>,
    bold: bool,
}

pub struct SimpleTable {
    headers: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Option<TableRow>>,
    style: TerminalStyle,
    terminal_width: usize,
    compact_dates: bool,
    bold_headers: bool,
}

impl SimpleTable {
    pub fn new(headers: Vec<&str>, aligns: Vec<Align>, style: impl Into<TerminalStyle>) -> Self {
        Self {
            headers: headers.into_iter().map(str::to_string).collect(),
            aligns,
            rows: Vec::new(),
            style: style.into(),
            terminal_width: DEFAULT_TERMINAL_WIDTH,
            compact_dates: false,
            bold_headers: false,
        }
    }

    pub fn with_terminal_width(mut self, width: usize) -> Self {
        self.terminal_width = width;
        self
    }

    pub fn with_date_compaction(mut self, compact_dates: bool) -> Self {
        self.compact_dates = compact_dates;
        self
    }

    pub fn with_bold_headers(mut self) -> Self {
        self.bold_headers = true;
        self
    }

    pub fn push(&mut self, row: Vec<String>) {
        self.rows.push(Some(TableRow {
            cells: row,
            cell_alignment: None,
            bold: false,
        }));
    }

    pub fn push_with_cell_alignment(&mut self, row: Vec<String>, column: usize, align: Align) {
        self.rows.push(Some(TableRow {
            cells: row,
            cell_alignment: Some((column, align)),
            bold: false,
        }));
    }

    pub fn bold_last_row(&mut self) {
        if let Some(Some(row)) = self.rows.last_mut() {
            row.bold = true;
        }
    }

    pub fn separator(&mut self) {
        self.rows.push(None);
    }

    pub fn column_count(&self) -> usize {
        self.headers.len()
    }

    pub fn print(&self) -> io::Result<()> {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        for line in self.render_lines() {
            writeln!(stdout, "{line}")?;
        }
        Ok(())
    }

    fn render_lines(&self) -> Vec<String> {
        let widths = self.column_widths();
        let header_aligns = vec![Align::Center; widths.len()];
        let mut lines = Vec::new();
        lines.push(border('┌', '┬', '┐', &widths));
        for header_row in expand_multiline_row(&self.headers, self.headers.len(), &widths) {
            let header_row = header_row
                .iter()
                .map(|header| {
                    let header = color(self.style, header, Color::Blue);
                    if self.bold_headers {
                        bold(self.style, &header)
                    } else {
                        header
                    }
                })
                .collect::<Vec<_>>();
            lines.push(table_line(&header_row, &header_aligns, &widths, None));
        }
        lines.push(border('├', '┼', '┤', &widths));
        for (row_index, row) in self.rows.iter().enumerate() {
            match row {
                Some(row) => {
                    let cells = self.compact_date_row(&row.cells, &widths);
                    for mut physical_row in
                        expand_multiline_row(&cells, self.headers.len(), &widths)
                    {
                        if row.bold {
                            for cell in &mut physical_row {
                                *cell = bold(self.style, cell);
                            }
                        }
                        lines.push(table_line(
                            &physical_row,
                            &self.aligns,
                            &widths,
                            row.cell_alignment,
                        ));
                    }
                }
                None => lines.push(border('├', '┼', '┤', &widths)),
            }
            if row.is_some()
                && row_index + 1 < self.rows.len()
                && !matches!(self.rows.get(row_index + 1), Some(None))
            {
                lines.push(border('├', '┼', '┤', &widths));
            }
        }
        lines.push(border('└', '┴', '┘', &widths));
        lines
    }

    fn column_widths(&self) -> Vec<usize> {
        let content_widths = self
            .headers
            .iter()
            .map(|header| visible_width_max_line(header))
            .collect::<Vec<_>>();
        let mut content_widths = content_widths;
        for row in self.rows.iter().flatten() {
            for (index, cell) in row.cells.iter().enumerate() {
                let cell_width = visible_width_max_line(cell);
                if let Some(width) = content_widths.get_mut(index) {
                    *width = (*width).max(cell_width);
                }
            }
        }
        let widths = content_widths
            .iter()
            .enumerate()
            .map(|(index, width)| {
                if self.aligns.get(index) == Some(&Align::Right) {
                    (width + 3).max(11)
                } else if index == 1 {
                    (width + 2).max(15)
                } else {
                    (width + 2).max(10)
                }
            })
            .collect::<Vec<_>>();
        let total_required = cli_table_required_width(&widths);
        let first_column_min = if self.compact_dates && total_required <= self.terminal_width {
            12
        } else {
            10
        };
        fit_widths_to_terminal(widths, &self.aligns, self.terminal_width, first_column_min)
    }

    fn compact_date_row(&self, row: &[String], widths: &[usize]) -> Vec<String> {
        if !self.compact_dates || widths.first().copied().unwrap_or_default() >= 12 {
            return row.to_vec();
        }
        let mut row = row.to_vec();
        if let Some(first) = row.first_mut()
            && let Some(compact) = compact_date_cell(first)
        {
            *first = compact;
        }
        row
    }
}

fn expand_multiline_row(row: &[String], column_count: usize, widths: &[usize]) -> Vec<Vec<String>> {
    let cells = (0..column_count)
        .map(|index| {
            let content_width = widths
                .get(index)
                .copied()
                .unwrap_or_default()
                .saturating_sub(2);
            row.get(index)
                .map(|cell| wrap_cell_lines(cell, content_width))
                .filter(|lines| !lines.is_empty())
                .unwrap_or_else(|| vec![String::new()])
        })
        .collect::<Vec<_>>();
    let height = cells.iter().map(Vec::len).max().unwrap_or(1);
    (0..height)
        .map(|line_index| {
            cells
                .iter()
                .map(|lines| lines.get(line_index).cloned().unwrap_or_default())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn fit_widths_to_terminal(
    mut widths: Vec<usize>,
    aligns: &[Align],
    terminal_width: usize,
    first_column_min: usize,
) -> Vec<usize> {
    if cli_table_required_width(&widths) <= terminal_width {
        return widths;
    }

    let minimums = widths
        .iter()
        .enumerate()
        .map(|(index, _)| {
            if aligns.get(index) == Some(&Align::Right) {
                10
            } else if index == 0 {
                first_column_min
            } else if index == 1 {
                12
            } else {
                8
            }
        })
        .collect::<Vec<_>>();

    // Smart shrink: keep Date (index 0) intact as long as possible.
    while cli_table_required_width(&widths) > terminal_width {
        let candidate = widths
            .iter()
            .enumerate()
            .filter(|(idx, w)| **w > minimums[*idx] && *idx != 0)
            .max_by_key(|(_, w)| **w)
            .map(|(idx, _)| idx)
            .or_else(|| {
                widths
                    .iter()
                    .enumerate()
                    .filter(|(idx, w)| **w > minimums[*idx])
                    .max_by_key(|(_, w)| **w)
                    .map(|(idx, _)| idx)
            });
        let Some(index) = candidate else {
            break;
        };
        widths[index] -= 1;
    }
    widths
}

fn cli_table_required_width(widths: &[usize]) -> usize {
    widths.iter().sum::<usize>() + widths.len() + 1
}

fn wrap_cell_lines(cell: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    for line in cell.lines() {
        if visible_width(line) <= width {
            lines.push(line.to_string());
        } else {
            lines.extend(wrap_cell_line(line, width));
        }
    }
    lines
}

fn wrap_cell_line(line: &str, width: usize) -> Vec<String> {
    let mut remaining = line.split_whitespace();
    let mut words = Vec::new();
    while let Some(word) = remaining.next() {
        if is_list_marker(word) {
            words.push(
                remaining
                    .next()
                    .map_or_else(|| word.to_string(), |next| format!("{word} {next}")),
            );
        } else {
            words.push(word.to_string());
        }
    }
    if words.len() <= 1 {
        return vec![truncate_to_width(line, width)];
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_source = String::new();
    for word in words {
        let candidate_width = if current.is_empty() {
            visible_width(&word)
        } else {
            visible_width(&current) + 1 + visible_width(&word)
        };
        if candidate_width <= width {
            if !current.is_empty() {
                current.push(' ');
                current_source.push(' ');
            }
            current.push_str(&word);
            current_source.push_str(&word);
        } else {
            if !current.is_empty() {
                lines.push((current, current_source));
            }
            if visible_width(&word) > width {
                current = truncate_to_width(&word, width);
                current_source = word;
            } else {
                current_source = word.clone();
                current = word;
            }
        }
    }
    if !current.is_empty() {
        lines.push((current, current_source));
    }
    let mut continuation = String::new();
    lines
        .into_iter()
        .map(|(line, source)| {
            let line = if continuation.is_empty() {
                line
            } else {
                format!("{continuation}{line}")
            };
            continuation = ansi_continuation(&format!("{continuation}{source}"));
            line
        })
        .collect()
}

fn is_list_marker(word: &str) -> bool {
    visible_width(word) == 1 && word.contains('-')
}

fn compact_date_cell(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
    {
        Some(format!("{}\n{}", &value[..4], &value[5..]))
    } else {
        None
    }
}

fn table_line(
    cells: &[String],
    aligns: &[Align],
    widths: &[usize],
    cell_alignment: Option<(usize, Align)>,
) -> String {
    let mut line = String::from("│");
    for (index, width) in widths.iter().enumerate() {
        let cell = cells.get(index).map(String::as_str).unwrap_or("");
        let align = if index == 0 && cell.starts_with("(assuming ") {
            Align::Right
        } else if let Some((column, align)) = cell_alignment
            && column == index
        {
            align
        } else {
            aligns.get(index).copied().unwrap_or(Align::Left)
        };
        line.push(' ');
        line.push_str(&pad_cell(cell, width.saturating_sub(2), align));
        line.push(' ');
        line.push('│');
    }
    line
}

fn pad_cell(cell: &str, width: usize, align: Align) -> String {
    let cell = ensure_ansi_reset(cell);
    let visible = visible_width(&cell);
    if visible >= width {
        return cell;
    }
    let padding = width - visible;
    match align {
        Align::Left => format!("{cell}{}", " ".repeat(padding)),
        Align::Center => {
            let left = padding / 2;
            format!("{}{cell}{}", " ".repeat(left), " ".repeat(padding - left))
        }
        Align::Right => format!("{}{cell}", " ".repeat(padding)),
    }
}

fn border(left: char, middle: char, right: char, widths: &[usize]) -> String {
    let mut line = String::new();
    line.push(left);
    for (index, width) in widths.iter().enumerate() {
        line.push_str(&"─".repeat(*width));
        line.push(if index + 1 == widths.len() {
            right
        } else {
            middle
        });
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn center_alignment_uses_visible_width_and_balanced_padding() {
        for (cell, width, expected) in [
            ("All", 9, "   All   "),
            ("All", 8, "  All   "),
            ("\x1b[32mAll", 9, "   \x1b[32mAll\x1b[0m   "),
            ("界", 7, "  界   "),
        ] {
            assert_eq!(pad_cell(cell, width, Align::Center), expected);
        }
    }

    #[test]
    fn compact_date_cell_splits_iso_dates() {
        assert_eq!(
            compact_date_cell("2026-05-18"),
            Some("2026\n05-18".to_string())
        );
        assert_eq!(compact_date_cell("20260518"), None);
    }

    #[test]
    fn width_fitting_keeps_table_within_terminal_when_possible() {
        let widths = fit_widths_to_terminal(
            vec![20, 40, 14, 14],
            &[Align::Left, Align::Left, Align::Right, Align::Right],
            60,
            12,
        );

        assert!(cli_table_required_width(&widths) <= 60);
    }

    #[test]
    fn numeric_columns_keep_the_reverted_minimum_when_space_is_tight() {
        let widths = fit_widths_to_terminal(
            vec![20, 40, 14, 14],
            &[Align::Left, Align::Left, Align::Right, Align::Right],
            49,
            12,
        );

        assert_eq!(widths[2], 10);
        assert_eq!(widths[3], 10);
    }

    #[test]
    fn snapshots_full_table_with_multiline_cells_and_separators() {
        let mut table = SimpleTable::new(
            vec!["Date", "Models", "Input", "Output", "Cost (USD)"],
            vec![
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
            TerminalStyle {
                no_color: true,
                ..TerminalStyle::default()
            },
        )
        .with_terminal_width(120);
        table.push(vec![
            "2026-05-18".to_string(),
            "- claude-sonnet-4\n- gpt-5.2-codex".to_string(),
            "1,234".to_string(),
            "56".to_string(),
            "$0.42".to_string(),
        ]);
        table.push(vec![
            "(assuming cache warmup)".to_string(),
            String::new(),
            "0".to_string(),
            "0".to_string(),
            "$0.00".to_string(),
        ]);
        table.separator();
        table.push(vec![
            "Total".to_string(),
            String::new(),
            "1,234".to_string(),
            "56".to_string(),
            "$0.42".to_string(),
        ]);

        insta::assert_snapshot!(table.render_lines().join("\n"));
    }

    #[test]
    fn snapshots_narrow_table_with_wrapping_truncation_and_compact_dates() {
        let mut table = SimpleTable::new(
            vec!["Date", "Models", "Input", "Output", "Cost (USD)"],
            vec![
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
            TerminalStyle {
                no_color: true,
                ..TerminalStyle::default()
            },
        )
        .with_terminal_width(56)
        .with_date_compaction(true);
        table.push(vec![
            "2026-05-18".to_string(),
            "- claude-sonnet-4-20250514\n- unusually-long-model-name-without-breaks".to_string(),
            "123,456,789".to_string(),
            "9,876,543".to_string(),
            "$12345.67".to_string(),
        ]);

        insta::assert_snapshot!(table.render_lines().join("\n"));
    }

    #[test]
    fn column_widths_uses_max_line_not_sum_for_multiline_cells() {
        let mut table = SimpleTable::new(
            vec!["Date", "Models", "Input", "Output", "Cost (USD)"],
            vec![
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
            TerminalStyle {
                no_color: true,
                ..TerminalStyle::default()
            },
        )
        .with_terminal_width(200);
        // 5 models — a realistic single-agent scenario where the bug would be severe
        table.push(vec![
            "2026-05-18".to_string(),
            "- claude-sonnet-4-20250514 (self-serve)\n- claude-opus-4-5\n- gpt-5.2-codex\n- gemini-3.0-pro-wildly-long\n- claude-haiku-3-5-sonnet".to_string(),
            "1,234".to_string(),
            "56".to_string(),
            "$0.42".to_string(),
        ]);
        let widths = table.column_widths();
        let models_width = widths[1];
        let cell = "- claude-sonnet-4-20250514 (self-serve)\n- claude-opus-4-5\n- gpt-5.2-codex\n- gemini-3.0-pro-wildly-long\n- claude-haiku-3-5-sonnet";
        let widest_line = visible_width_max_line(cell);
        let sum_of_lines = cell.lines().map(visible_width).sum::<usize>();
        // If visible_width_sum were still used, models_width would be ~180
        // With visible_width_max_line, it should be ~widest_line + padding
        assert!(
            models_width < sum_of_lines,
            "Models column width ({models_width}) should be based on widest line ({widest_line}), not sum of all lines ({sum_of_lines})"
        );
        assert!(
            models_width <= widest_line + 3,
            "Models width ({models_width}) should be close to widest line width ({widest_line}), not {sum_of_lines}"
        );
    }

    #[test]
    fn expands_wide_models_column_when_space_allows() {
        let mut table = SimpleTable::new(
            vec!["Date", "Models", "Input", "Output", "Total Tokens"],
            vec![
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
            TerminalStyle {
                no_color: true,
                ..TerminalStyle::default()
            },
        )
        .with_terminal_width(120);
        table.push(vec![
            "2026-05-18".to_string(),
            "- provider/this-is-a-deliberately-wide-model-name".to_string(),
            "13,044".to_string(),
            "125,061".to_string(),
            "43,633".to_string(),
        ]);
        table.separator();
        table.push(vec![
            "Total".to_string(),
            String::new(),
            "99,999,999".to_string(),
            "88,888,888".to_string(),
            "77,777,777".to_string(),
        ]);

        let widths = table.column_widths();
        assert_eq!(widths[1], 51);

        let rendered = table.render_lines().join("\n");
        assert!(rendered.contains("- provider/this-is-a-deliberately-wide-model-name"));
        assert!(rendered.contains("99,999,999"));
        assert!(rendered.contains("88,888,888"));
        assert!(rendered.contains("77,777,777"));
        assert!(rendered.contains("43,633"));
    }

    #[test]
    fn expands_models_column_after_agent_column() {
        let mut table = SimpleTable::new(
            vec!["Date", "Agent", "Models", "Input", "Output", "Total Tokens"],
            vec![
                Align::Left,
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
            TerminalStyle {
                no_color: true,
                ..TerminalStyle::default()
            },
        )
        .with_terminal_width(120);
        table.push(vec![
            "2026-05-18".to_string(),
            "codex".to_string(),
            "- provider/this-is-a-deliberately-wide-model-name".to_string(),
            "13,044".to_string(),
            "125,061".to_string(),
            "43,633".to_string(),
        ]);

        let widths = table.column_widths();
        assert_eq!(widths[2], 49);
        assert!(table.render_lines().join("\n").contains("codex"));
    }

    #[test]
    fn expands_models_and_truncates_numerics_to_fit() {
        let mut table = SimpleTable::new(
            vec![
                "Date",
                "Models",
                "Input",
                "Output",
                "Reasoning",
                "Cache Read",
                "Total Tokens",
                "Cost (USD)",
            ],
            vec![
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
            TerminalStyle {
                no_color: true,
                ..TerminalStyle::default()
            },
        )
        .with_terminal_width(120);
        table.push(vec![
            "2026-05-18".to_string(),
            "- provider/this-is-a-deliberately-wide-model-name".to_string(),
            "1,234".to_string(),
            "5,678".to_string(),
            "9,012".to_string(),
            "3,456".to_string(),
            "7,890".to_string(),
            "$12.34".to_string(),
        ]);
        table.separator();
        table.push(vec![
            "Total".to_string(),
            String::new(),
            "99,999,999".to_string(),
            "88,888,888".to_string(),
            "77,777,777".to_string(),
            "66,666,666".to_string(),
            "987,654,321".to_string(),
            "$12345.67".to_string(),
        ]);

        let widths = table.column_widths();
        assert!(widths[1] > 12, "{widths:?}");
        assert!(cli_table_required_width(&widths) <= 120, "{widths:?}");

        let rendered = table.render_lines().join("\n");
        assert!(rendered.lines().all(|line| visible_width(line) <= 120));
    }

    #[test]
    fn expands_models_and_truncates_large_totals_to_fit() {
        let mut table = SimpleTable::new(
            vec![
                "Date",
                "Models",
                "Input",
                "Output",
                "Reasoning",
                "Cache Create",
                "Cache Read",
                "Total Tokens",
                "Cost (USD)",
            ],
            vec![
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
            TerminalStyle {
                no_color: true,
                ..TerminalStyle::default()
            },
        )
        .with_terminal_width(120)
        .with_date_compaction(true);
        table.push(vec![
            "2026-05-18".to_string(),
            "- provider/this-is-a-deliberately-wide-model-name".to_string(),
            "13,044,466".to_string(),
            "125,061".to_string(),
            "97,285".to_string(),
            "30,366,617".to_string(),
            "43,633,429".to_string(),
            "123,456,789".to_string(),
            "$12.34".to_string(),
        ]);
        table.separator();
        table.push(vec![
            "Total".to_string(),
            String::new(),
            "99,999,999".to_string(),
            "8,888,888".to_string(),
            "777,777".to_string(),
            "66,666,666".to_string(),
            "777,777,777".to_string(),
            "1,048,000,000".to_string(),
            "$123456.78".to_string(),
        ]);

        let widths = table.column_widths();
        assert_eq!(cli_table_required_width(&widths), 120);

        let rendered = table.render_lines().join("\n");
        assert!(
            rendered.lines().all(|line| visible_width(line) == 120),
            "{rendered}"
        );
        let total_line = rendered
            .lines()
            .find(|line| line.starts_with("│ Total"))
            .expect("rendered table should include a Total row");
        assert!(total_line.contains('…'), "{total_line}");
        assert!(total_line.contains("Total"), "{total_line}");
    }

    #[test]
    fn expands_models_column_at_the_codex_120_column_boundary() {
        let mut table = SimpleTable::new(
            vec![
                "Date",
                "Models",
                "Input",
                "Output",
                "Reasoning",
                "Cache Create",
                "Cache Read",
                "Total Tokens",
                "Cost (USD)",
            ],
            vec![
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
            TerminalStyle {
                no_color: true,
                ..TerminalStyle::default()
            },
        )
        .with_terminal_width(120)
        .with_date_compaction(true);
        table.push(vec![
            "2026-05-18".to_string(),
            "- provider/this-is-a-deliberately-wide-model-name".to_string(),
            "123,456,789".to_string(),
            "1,234,567".to_string(),
            "9,876,543".to_string(),
            "1,234,567".to_string(),
            "12,345,678,901".to_string(),
            "12,345,678,901".to_string(),
            "$12345.67".to_string(),
        ]);
        table.separator();
        table.push(vec![
            "Total".to_string(),
            String::new(),
            "123,456,789".to_string(),
            "1,234,567".to_string(),
            "9,876,543".to_string(),
            "1,234,567".to_string(),
            "12,345,678,901".to_string(),
            "12,345,678,901".to_string(),
            "$12345.67".to_string(),
        ]);

        let widths = table.column_widths();
        assert_eq!(cli_table_required_width(&widths), 120);
        assert!(widths[1] > 12, "{widths:?}");

        let rendered = table.render_lines().join("\n");
        let total_line = rendered
            .lines()
            .find(|line| line.starts_with("│ Total"))
            .expect("rendered table should include a Total row");
        assert!(total_line.contains('…'), "{total_line}");
        for value in ["123,456,789", "12,345,678,901"] {
            assert!(
                rendered.contains(value) || total_line.contains('…'),
                "missing {value} in {total_line}"
            );
        }
    }

    #[test]
    fn resets_colored_models_before_each_physical_cell_border() {
        let mut table = SimpleTable::new(
            vec!["Date", "Models", "Input"],
            vec![Align::Left, Align::Left, Align::Right],
            TerminalStyle {
                color: true,
                ..TerminalStyle::default()
            },
        )
        .with_terminal_width(120);
        table.push(vec![
            "2026-05-18".to_string(),
            "\x1b[32m- short\n\x1b[32m- provider/this-is-a-deliberately-wide-model-name"
                .to_string(),
            "1,234".to_string(),
        ]);

        let rendered = table.render_lines();
        assert_eq!(
            rendered
                .iter()
                .filter(|line| line.contains("- short") || line.contains("provider/this-is"))
                .count(),
            2,
            "{rendered:?}"
        );
        for line in &rendered {
            for cell in line.split('│') {
                let cell = cell.trim_end();
                if cell.contains("\x1b[") {
                    assert!(cell.ends_with("\x1b[0m"), "{line:?}");
                }
            }
        }
    }

    #[test]
    fn keeps_date_and_numeric_columns_readable_at_eighty_columns() {
        let mut table = SimpleTable::new(
            vec!["Date", "Models", "Input", "Output", "Total Tokens"],
            vec![
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
            TerminalStyle {
                no_color: true,
                ..TerminalStyle::default()
            },
        )
        .with_terminal_width(80)
        .with_date_compaction(true);
        table.push(vec![
            "2026-05-18".to_string(),
            "- provider/this-is-a-deliberately-wide-model-name".to_string(),
            "13,044".to_string(),
            "125,061".to_string(),
            "43,633".to_string(),
        ]);
        table.separator();
        table.push(vec![
            "Total".to_string(),
            String::new(),
            "99,999,999".to_string(),
            "88,888,888".to_string(),
            "77,777,777".to_string(),
        ]);

        let rendered = table.render_lines().join("\n");
        assert!(rendered.lines().all(|line| visible_width(line) <= 80));
        assert!(rendered.contains("2026"));
        assert!(rendered.contains("05-18"));
        assert!(rendered.contains("13,044"));
        assert!(rendered.contains("99,999,999"));
        assert!(rendered.contains("88,888,888"));
        assert!(rendered.contains("77,777,777"));
        assert!(rendered.contains("2026-05-18"));
    }

    #[test]
    fn measures_ansi_multiline_models_by_visible_width() {
        let mut table = SimpleTable::new(
            vec!["Date", "Models", "Input"],
            vec![Align::Left, Align::Left, Align::Right],
            TerminalStyle {
                no_color: true,
                ..TerminalStyle::default()
            },
        )
        .with_terminal_width(120);
        table.push(vec![
            "2026-05-18".to_string(),
            "\x1b[32m- 表表表表表表表表表表表表表表\x1b[0m\n- short".to_string(),
            "1,234".to_string(),
        ]);

        let widths = table.column_widths();
        assert_eq!(widths[1], 32);

        let rendered = table.render_lines().join("\n");
        assert!(rendered.lines().all(|line| visible_width(line) <= 120));
    }

    #[test]
    fn compact_date_layout_remains_stable_with_a_long_model() {
        let mut table = SimpleTable::new(
            vec!["Date", "Models", "Input", "Output", "Cost (USD)"],
            vec![
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
            TerminalStyle {
                no_color: true,
                ..TerminalStyle::default()
            },
        )
        .with_terminal_width(56)
        .with_date_compaction(true);
        table.push(vec![
            "2026-05-18".to_string(),
            "- provider/this-is-a-deliberately-wide-model-name".to_string(),
            "123,456,789".to_string(),
            "9,876,543".to_string(),
            "$12345.67".to_string(),
        ]);

        let rendered = table.render_lines().join("\n");
        assert!(rendered.contains("2026"));
        assert!(rendered.contains("05-18"));
        assert!(rendered.contains("Models"));
        assert!(rendered.contains("Input"));
        assert!(rendered.contains("Cost"));
    }

    #[test]
    fn preserves_status_markers_when_models_is_not_the_second_column() {
        let mut table = SimpleTable::new(
            vec!["Block Start", "Duration/Status", "Models", "Tokens", "Cost"],
            vec![
                Align::Left,
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
            ],
            TerminalStyle {
                no_color: true,
                ..TerminalStyle::default()
            },
        )
        .with_terminal_width(56);
        table.push(vec![
            "08/31, 10:00 AM".to_string(),
            "(inactive)".to_string(),
            "- gpt-5".to_string(),
            "123".to_string(),
            "$1.23".to_string(),
        ]);

        let rendered = table.render_lines().join("\n");
        let status_line = rendered
            .lines()
            .find(|line| line.contains("(inactive)"))
            .expect("status marker should remain visible");
        let status_cell = status_line
            .split('│')
            .nth(2)
            .expect("rendered row should include the status cell");
        assert!(!status_cell.contains('…'), "{status_line}");
        assert!(rendered.lines().all(|line| visible_width(line) <= 56));
    }

    #[test]
    fn resets_ansi_continuation_at_explicit_newlines() {
        let mut table = SimpleTable::new(
            vec!["Date", "Models", "Input"],
            vec![Align::Left, Align::Left, Align::Right],
            TerminalStyle {
                no_color: true,
                ..TerminalStyle::default()
            },
        )
        .with_terminal_width(120);
        table.push(vec![
            "2026-05-18".to_string(),
            "\x1b[32m- first\n- second".to_string(),
            "1,234".to_string(),
        ]);

        let rendered = table.render_lines();
        let model_lines = rendered
            .iter()
            .filter(|line| line.contains("- first") || line.contains("- second"))
            .collect::<Vec<_>>();
        assert_eq!(model_lines.len(), 2, "{rendered:?}");
        let first_cell = model_lines[0]
            .split('│')
            .nth(2)
            .expect("rendered row should include the Models cell");
        let second_cell = model_lines[1]
            .split('│')
            .nth(2)
            .expect("rendered row should include the Models cell");
        assert!(first_cell.contains("\x1b[32m"), "{rendered:?}");
        assert!(first_cell.trim_end().ends_with("\x1b[0m"), "{rendered:?}");
        assert!(!second_cell.contains("\x1b[32m"), "{rendered:?}");
    }

    #[test]
    fn keeps_list_markers_attached_to_truncated_models() {
        let mut table = SimpleTable::new(
            vec!["Date", "Models", "Input"],
            vec![Align::Left, Align::Left, Align::Right],
            TerminalStyle {
                no_color: true,
                ..TerminalStyle::default()
            },
        )
        .with_terminal_width(56);
        table.push(vec![
            "2026-05-18".to_string(),
            "- provider/very-long-model-name".to_string(),
            "12345678901".to_string(),
        ]);

        let rendered = table.render_lines().join("\n");
        assert!(
            rendered.lines().any(|line| {
                line.split('│')
                    .nth(2)
                    .is_some_and(|cell| cell.contains("- provider/"))
            }),
            "{rendered}"
        );
        assert!(!rendered.lines().any(|line| {
            line.split('│')
                .nth(2)
                .is_some_and(|cell| cell.trim() == "-")
        }));
    }

    #[test]
    fn preserves_ansi_continuation_across_word_wrapped_fragments_without_leaking() {
        let mut table = SimpleTable::new(
            vec!["Date", "Models", "Input"],
            vec![Align::Left, Align::Left, Align::Right],
            TerminalStyle {
                no_color: true,
                ..TerminalStyle::default()
            },
        )
        .with_terminal_width(56);
        table.push(vec![
            "2026-05-18".to_string(),
            "\x1b[32m- provider/foo long-model-name\x1b[0m".to_string(),
            "1,234".to_string(),
        ]);

        let rendered = table.render_lines();
        let model_lines = rendered
            .iter()
            .filter(|line| line.contains("provider/foo") || line.contains("long-model-name"))
            .collect::<Vec<_>>();
        assert_eq!(model_lines.len(), 2, "{rendered:?}");
        for line in model_lines {
            let cells = line.split('│').collect::<Vec<_>>();
            let model_cell = cells
                .get(2)
                .expect("rendered row should include the Models cell");
            let input_cell = cells
                .get(3)
                .expect("rendered row should include the Input cell");
            assert!(model_cell.contains("\x1b[32m"), "{line:?}");
            assert!(model_cell.trim_end().ends_with("\x1b[0m"), "{line:?}");
            assert!(!input_cell.contains("\x1b[32m"), "{line:?}");
        }
    }
}

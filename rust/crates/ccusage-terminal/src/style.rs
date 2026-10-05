use std::{
    env,
    io::{self, IsTerminal},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Color {
    Blue,
    Green,
    Grey,
    Red,
    Yellow,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalStyle {
    pub color: bool,
    pub log_level: Option<u8>,
    pub no_color: bool,
}

pub fn color(style: impl Into<TerminalStyle>, value: impl AsRef<str>, color: Color) -> String {
    let style = style.into();
    let value = value.as_ref();
    if !use_color(&style) {
        return value.to_string();
    }
    let code = match color {
        Color::Blue => 34,
        Color::Green => 32,
        Color::Grey => 90,
        Color::Red => 31,
        Color::Yellow => 33,
    };
    format!("\x1b[{code}m{value}\x1b[0m")
}

pub(crate) fn bold(style: TerminalStyle, value: &str) -> String {
    if value.is_empty() || !use_terminal_style(&style) {
        return value.to_string();
    }
    format!("\x1b[1m{value}\x1b[0m")
}

fn use_color(style: &TerminalStyle) -> bool {
    if style.no_color || env::var_os("NO_COLOR").is_some() {
        return false;
    }
    use_terminal_style(style)
}

fn use_terminal_style(style: &TerminalStyle) -> bool {
    style.color || env::var_os("FORCE_COLOR").is_some() || io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_remains_enabled_when_colors_are_disabled() {
        let style = TerminalStyle {
            color: true,
            no_color: true,
            ..TerminalStyle::default()
        };

        assert_eq!(bold(style, "Total"), "\x1b[1mTotal\x1b[0m");
    }

    #[test]
    fn color_respects_explicit_no_color() {
        let style = TerminalStyle {
            color: true,
            log_level: None,
            no_color: true,
        };

        assert_eq!(color(style, "Total", Color::Yellow), "Total");
    }
}

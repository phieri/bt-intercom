// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Terminal color capability detection and semantic colors.

use std::env;

use ratatui::style::{Color, Style};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColorLevel {
	None,
	Ansi16,
	Ansi256,
	TrueColor,
}

#[derive(Clone, Copy, Debug)]
pub struct Palette {
	level: ColorLevel,
}

#[derive(Clone, Copy)]
pub enum Meaning {
	Success,
	Error,
	Warning,
	Info,
	Selection,
}

impl Palette {
	pub fn detect(is_terminal: bool) -> Self {
		let no_color = env::var_os("NO_COLOR").is_some();
		let term = env::var("TERM").unwrap_or_default();
		let colorterm = env::var("COLORTERM").unwrap_or_default();
		Self {
			level: color_level(is_terminal, no_color, &term, &colorterm),
		}
	}

	pub fn color(self, meaning: Meaning) -> Color {
		if self.level == ColorLevel::None {
			return Color::Reset;
		}
		match (self.level, meaning) {
			(ColorLevel::TrueColor, Meaning::Success) => Color::Rgb(0, 160, 0),
			(ColorLevel::TrueColor, Meaning::Error) => Color::Rgb(200, 40, 40),
			(ColorLevel::TrueColor, Meaning::Warning) => Color::Rgb(180, 120, 0),
			(ColorLevel::TrueColor, Meaning::Info) => Color::Rgb(40, 100, 200),
			(ColorLevel::TrueColor, Meaning::Selection) => Color::Rgb(0, 140, 160),
			(ColorLevel::Ansi256, Meaning::Success) => Color::Indexed(2),
			(ColorLevel::Ansi256, Meaning::Error) => Color::Indexed(1),
			(ColorLevel::Ansi256, Meaning::Warning) => Color::Indexed(3),
			(ColorLevel::Ansi256, Meaning::Info) => Color::Indexed(4),
			(ColorLevel::Ansi256, Meaning::Selection) => Color::Indexed(6),
			(_, Meaning::Success) => Color::Green,
			(_, Meaning::Error) => Color::Red,
			(_, Meaning::Warning) => Color::Yellow,
			(_, Meaning::Info) => Color::Blue,
			(_, Meaning::Selection) => Color::Cyan,
		}

		pub fn style(self, meaning: Meaning) -> Style {
			if self.level == ColorLevel::None {
				Style::default()
			} else {
				Style::default().fg(self.color(meaning))
			}
		}
	}
}

fn color_level(is_terminal: bool, no_color: bool, term: &str, colorterm: &str) -> ColorLevel {
	if !is_terminal || no_color || term.eq_ignore_ascii_case("dumb") {
		ColorLevel::None
	} else if colorterm.eq_ignore_ascii_case("truecolor")
		|| colorterm.eq_ignore_ascii_case("24bit")
	{
		ColorLevel::TrueColor
	} else if term.to_ascii_lowercase().contains("256color") {
		ColorLevel::Ansi256
	} else {
		ColorLevel::Ansi16
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn color_level_respects_terminal_and_no_color() {
		assert_eq!(color_level(false, false, "xterm-256color", ""), ColorLevel::None);
		assert_eq!(color_level(true, true, "xterm-256color", "truecolor"), ColorLevel::None);
		assert_eq!(color_level(true, false, "dumb", ""), ColorLevel::None);
		assert_eq!(color_level(true, false, "xterm-256color", ""), ColorLevel::Ansi256);
		assert_eq!(color_level(true, false, "xterm", "24bit"), ColorLevel::TrueColor);
		assert_eq!(color_level(true, false, "xterm", ""), ColorLevel::Ansi16);
	}

	#[test]
	fn monochrome_palette_does_not_emit_colors() {
		let palette = Palette {
			level: ColorLevel::None,
		};
		assert_eq!(palette.color(Meaning::Success), Color::Reset);
		assert_eq!(palette.color(Meaning::Error), Color::Reset);
	}
}

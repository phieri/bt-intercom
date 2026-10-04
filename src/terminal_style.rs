// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Terminal color capability detection and semantic colors.

use std::{env, ffi::OsString};

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
		let no_color = no_color_requested(env::var_os("NO_COLOR"));
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
	}

	pub fn style(self, meaning: Meaning) -> Style {
		if self.level == ColorLevel::None {
			Style::default()
		} else {
			Style::default().fg(self.color(meaning))
		}
	}
}

fn no_color_requested(value: Option<OsString>) -> bool {
	value.is_some_and(|value| !value.is_empty())
}

fn color_level(is_terminal: bool, no_color: bool, term: &str, colorterm: &str) -> ColorLevel {
	if !is_terminal || no_color || term.eq_ignore_ascii_case("dumb") {
		ColorLevel::None
	} else if colorterm.eq_ignore_ascii_case("truecolor") || colorterm.eq_ignore_ascii_case("24bit")
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
	fn no_color_requires_a_non_empty_value() {
		for (value, expected) in [
			(None, false),
			(Some(OsString::new()), false),
			(Some(OsString::from("1")), true),
		] {
			assert_eq!(no_color_requested(value), expected);
		}
	}

	#[test]
	fn detects_color_level_from_terminal_capabilities() {
		for (terminal, no_color, term, colorterm, expected) in [
			(false, false, "xterm-256color", "", ColorLevel::None),
			(true, true, "xterm-256color", "truecolor", ColorLevel::None),
			(true, false, "DUMB", "", ColorLevel::None),
			(true, false, "xterm-256color", "", ColorLevel::Ansi256),
			(true, false, "XTERM-256COLOR", "", ColorLevel::Ansi256),
			(true, false, "xterm", "24bit", ColorLevel::TrueColor),
			(true, false, "xterm", "TRUECOLOR", ColorLevel::TrueColor),
			(
				true,
				false,
				"xterm-256color",
				"24bit",
				ColorLevel::TrueColor,
			),
			(true, false, "xterm", "", ColorLevel::Ansi16),
		] {
			assert_eq!(
				color_level(terminal, no_color, term, colorterm),
				expected,
				"{terminal:?} {no_color:?} {term:?} {colorterm:?}"
			);
		}
	}

	#[test]
	fn monochrome_palette_does_not_emit_colors() {
		let palette = Palette {
			level: ColorLevel::None,
		};
		for meaning in [
			Meaning::Success,
			Meaning::Error,
			Meaning::Warning,
			Meaning::Info,
			Meaning::Selection,
		] {
			assert_eq!(palette.color(meaning), Color::Reset);
		}
	}
}

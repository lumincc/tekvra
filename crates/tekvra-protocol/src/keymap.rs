//! Plugin keybinding grammar shared by the Luau plugin host (registration-time validation)
//! and the TUI (terminal key matching). It is deliberately independent of any terminal
//! event library so both crates agree on one notation.

use std::fmt;

/// A single physical key a [`KeyChord`] can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeySpec {
    /// One ASCII character, stored lowercase (terminals report shifted letters as uppercase).
    Char(char),
    /// Function key `F1`–`F12`.
    Function(u8),
}

impl KeySpec {
    /// Parses the key part of a chord (`g`, `F5`, `1`, `=`), case-insensitively.
    fn parse(part: &str) -> Result<Self, String> {
        let lower = part.to_ascii_lowercase();
        if let Some(digits) = lower.strip_prefix('f')
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            let Ok(n) = digits.parse::<u8>() else {
                return Err("function keys are `F1`–`F12`".to_string());
            };
            if !(1..=12).contains(&n) {
                return Err("function keys are `F1`–`F12`".to_string());
            }
            return Ok(Self::Function(n));
        }
        let mut chars = part.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return Err(format!(
                "`{part}` is not a single key (named keys such as Tab, Esc, and Enter are reserved for typing and navigation; use a `Ctrl`/`Alt` chord or a function key)"
            ));
        };
        if !c.is_ascii() {
            return Err(format!("`{part}` is not an ASCII key"));
        }
        Ok(Self::Char(c.to_ascii_lowercase()))
    }
}

/// Chords the TUI owns; plugins cannot bind them.
const RESERVED_CHORDS: [(KeyChord, &str); 3] = [
    (
        KeyChord {
            ctrl: true,
            alt: false,
            key: KeySpec::Char('c'),
        },
        "interrupt / quit",
    ),
    (
        KeyChord {
            ctrl: true,
            alt: false,
            key: KeySpec::Char('x'),
        },
        "tasks overlay",
    ),
    (
        KeyChord {
            ctrl: true,
            alt: false,
            key: KeySpec::Char('j'),
        },
        "newline insert",
    ),
];

/// A `Ctrl` / `Alt` modifier combination plus one [`KeySpec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    /// `Ctrl` held.
    pub ctrl: bool,
    /// `Alt` (or `Meta`) held.
    pub alt: bool,
    /// The key itself.
    pub key: KeySpec,
}

impl KeyChord {
    /// Builds a chord, returning `None` for combinations plugins cannot bind: a bare
    /// character (it would swallow typed text).
    #[must_use]
    pub fn new(ctrl: bool, alt: bool, key: KeySpec) -> Option<Self> {
        if !ctrl && !alt && matches!(key, KeySpec::Char(_)) {
            return None;
        }
        Some(Self { ctrl, alt, key })
    }

    /// Returns the built-in the TUI binds this chord to, if any.
    #[must_use]
    pub fn reserved_for(&self) -> Option<&'static str> {
        RESERVED_CHORDS
            .iter()
            .find(|(chord, _)| chord == self)
            .map(|(_, owner)| *owner)
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<")?;
        if self.ctrl {
            write!(f, "C-")?;
        }
        if self.alt {
            write!(f, "A-")?;
        }
        match self.key {
            KeySpec::Char(c) => write!(f, "{c}")?,
            KeySpec::Function(n) => write!(f, "F{n}")?,
        }
        write!(f, ">")
    }
}

/// Parses the `lhs` notation accepted by `tekvra.keymap.set`: `<C-x>`, `<A-x>` / `<M-x>`,
/// `<C-A-x>`, `<F1>`–`<F12>`, `<C-F5>`. The `Err` string is the reason, surfaced verbatim in
/// plugin load notices.
pub fn parse_key_chord(lhs: &str) -> Result<KeyChord, String> {
    let text = lhs.trim();
    let Some(inner) = text
        .strip_prefix('<')
        .and_then(|t| t.strip_suffix('>'))
        .map(str::trim)
    else {
        return Err(format!(
            "invalid key chord `{lhs}`: expected `<...>` notation such as `<C-g>` or `<F2>`"
        ));
    };
    if inner.is_empty() || inner.contains(['<', '>']) {
        return Err(format!("invalid key chord `{lhs}`: name exactly one key"));
    }
    let parts: Vec<&str> = inner
        .split('-')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let (key_part, modifiers) = parts
        .split_last()
        .ok_or_else(|| format!("invalid key chord `{lhs}`: name exactly one key"))?;
    let mut ctrl = false;
    let mut alt = false;
    for modifier in modifiers {
        match modifier.to_ascii_uppercase().as_str() {
            "C" | "CTRL" | "CONTROL" => ctrl = true,
            "A" | "ALT" | "M" | "META" => alt = true,
            "S" | "SHIFT" => {
                return Err(format!(
                    "invalid key chord `{lhs}`: `Shift` cannot be bound (terminals report shifted letters as uppercase characters)"
                ));
            }
            other => {
                return Err(format!(
                    "invalid key chord `{lhs}`: unknown modifier `{other}` (expected C/CTRL or A/ALT/M/META)"
                ));
            }
        }
    }
    let key = KeySpec::parse(key_part)
        .map_err(|reason| format!("invalid key chord `{lhs}`: {reason}"))?;
    let chord = KeyChord::new(ctrl, alt, key).ok_or_else(|| {
        format!(
            "invalid key chord `{lhs}`: a bare key cannot be bound (it would swallow typed text); use a `Ctrl`/`Alt` chord or a function key"
        )
    })?;
    if let Some(owner) = chord.reserved_for() {
        return Err(format!("invalid key chord `{lhs}`: reserved for {owner}"));
    }
    Ok(chord)
}

/// Composer state a keybinding applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeymapMode {
    /// Fires only while the composer is empty (`"n"`).
    Normal,
    /// Fires only while a draft is being edited (`"i"`).
    Insert,
    /// Fires in both states (`"*"`).
    Both,
}

impl KeymapMode {
    /// Parses `"n"`, `"i"`, or `"*"`.
    pub fn parse(mode: &str) -> Result<Self, String> {
        match mode {
            "n" => Ok(Self::Normal),
            "i" => Ok(Self::Insert),
            "*" => Ok(Self::Both),
            other => Err(format!(
                "invalid keymap mode `{other}` (expected `n`, `i`, or `*`)"
            )),
        }
    }

    /// Canonical `"n"` / `"i"` / `"*"` notation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "n",
            Self::Insert => "i",
            Self::Both => "*",
        }
    }

    /// Whether the binding applies given whether the composer is empty.
    #[must_use]
    pub fn matches_draft(&self, draft_empty: bool) -> bool {
        match self {
            Self::Normal => draft_empty,
            Self::Insert => !draft_empty,
            Self::Both => true,
        }
    }
}

/// One validated keybinding registered by a plugin via `tekvra.keymap.set`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapSpec {
    /// Composer states the binding applies to.
    pub mode: KeymapMode,
    /// Chord that triggers the binding.
    pub chord: KeyChord,
    /// Slash command dispatched when the chord is pressed (for example `/review`).
    pub command: String,
}

impl KeymapSpec {
    /// Validates and parses a `tekvra.keymap.set(mode, lhs, command)` registration.
    pub fn parse(mode: &str, lhs: &str, command: &str) -> Result<Self, String> {
        let mode = KeymapMode::parse(mode)?;
        let chord = parse_key_chord(lhs)?;
        let command = command.trim();
        let name = command
            .strip_prefix('/')
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_default();
        let name_ok = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !name_ok {
            return Err(format!(
                "invalid keymap command `{command}`: expected a slash command such as `/review` or `/hello world`"
            ));
        }
        Ok(Self {
            mode,
            chord,
            command: command.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyChord, KeySpec, KeymapMode, KeymapSpec, parse_key_chord};

    const fn ctrl(c: char) -> KeyChord {
        KeyChord {
            ctrl: true,
            alt: false,
            key: KeySpec::Char(c),
        }
    }

    #[test]
    fn chord_parser_accepts_ctrl_alt_and_function_keys_case_insensitively() {
        let cases: [(&str, KeyChord); 8] = [
            ("<C-g>", ctrl('g')),
            ("<c-G>", ctrl('g')),
            (
                "<A-m>",
                KeyChord {
                    ctrl: false,
                    alt: true,
                    key: KeySpec::Char('m'),
                },
            ),
            (
                "<M-x>",
                KeyChord {
                    ctrl: false,
                    alt: true,
                    key: KeySpec::Char('x'),
                },
            ),
            (
                "<C-A-x>",
                KeyChord {
                    ctrl: true,
                    alt: true,
                    key: KeySpec::Char('x'),
                },
            ),
            (
                "<C-F5>",
                KeyChord {
                    ctrl: true,
                    alt: false,
                    key: KeySpec::Function(5),
                },
            ),
            (
                "<F12>",
                KeyChord {
                    ctrl: false,
                    alt: false,
                    key: KeySpec::Function(12),
                },
            ),
            ("< Ctrl - g >", ctrl('g')),
        ];
        for (text, expected) in cases {
            let parsed = parse_key_chord(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(parsed, expected, "{text}");
            // Display round-trips into the same chord.
            assert_eq!(
                parse_key_chord(&parsed.to_string()).ok(),
                Some(parsed),
                "{}",
                parsed
            );
        }
    }

    #[test]
    fn chord_parser_rejects_bare_named_shift_unknown_and_malformed_input() {
        let cases: [(&str, &str); 15] = [
            ("<a>", "bare key"),
            ("a", "expected `<...>`"),
            ("<S-a>", "`Shift` cannot be bound"),
            ("<Tab>", "not a single key"),
            ("<Esc>", "not a single key"),
            ("<Enter>", "not a single key"),
            ("gg", "expected `<...>`"),
            ("<C-x><C-s>", "name exactly one key"),
            ("<F0>", "`F1`–`F12`"),
            ("<F13>", "`F1`–`F12`"),
            ("<C-g> junk", "expected `<...>`"),
            ("<Q-g>", "unknown modifier `Q`"),
            ("< >", "name exactly one key"),
            ("C-g", "expected `<...>`"),
            ("<é>", "not an ASCII key"),
        ];
        for (text, expected) in cases {
            let err = parse_key_chord(text).expect_err(text);
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn reserved_built_in_chords_name_their_owner() {
        for (text, owner) in [
            ("<C-c>", "interrupt / quit"),
            ("<C-x>", "tasks overlay"),
            ("<C-j>", "newline insert"),
        ] {
            let err = parse_key_chord(text).expect_err(text);
            assert!(
                err.contains("reserved for") && err.contains(owner),
                "{text}: {err}"
            );
        }
        // `Alt` variants of the same letters are free.
        assert!(parse_key_chord("<A-c>").is_ok());
        assert!(parse_key_chord("<A-j>").is_ok());
    }

    #[test]
    fn chord_constructor_refuses_bare_characters_only() {
        assert!(KeyChord::new(false, false, KeySpec::Char('g')).is_none());
        assert!(KeyChord::new(false, false, KeySpec::Function(2)).is_some());
        assert!(KeyChord::new(false, true, KeySpec::Char('g')).is_some());
    }

    #[test]
    fn mode_parse_and_draft_matching() {
        assert_eq!(KeymapMode::parse("n").ok(), Some(KeymapMode::Normal));
        assert_eq!(KeymapMode::parse("i").ok(), Some(KeymapMode::Insert));
        assert_eq!(KeymapMode::parse("*").ok(), Some(KeymapMode::Both));
        assert!(KeymapMode::parse("v").is_err());
        assert!(KeymapMode::Normal.matches_draft(true));
        assert!(!KeymapMode::Normal.matches_draft(false));
        assert!(!KeymapMode::Insert.matches_draft(true));
        assert!(KeymapMode::Insert.matches_draft(false));
        assert!(KeymapMode::Both.matches_draft(true));
        assert!(KeymapMode::Both.matches_draft(false));
    }

    #[test]
    fn keymap_spec_validates_the_whole_registration() {
        let spec = KeymapSpec::parse("*", "<C-g>", " /hello world ").expect("valid spec");
        assert_eq!(spec.command, "/hello world");
        assert_eq!(spec.chord, ctrl('g'));
        assert_eq!(spec.mode, KeymapMode::Both);
        assert!(KeymapSpec::parse("n", "<C-g>", "/review").is_ok());

        for (mode, lhs, command, expected) in [
            ("v", "<C-g>", "/review", "invalid keymap mode `v`"),
            ("n", "<C-c>", "/review", "reserved for"),
            ("n", "<a>", "/review", "bare key"),
            ("n", "<C-g>", "ping", "expected a slash command"),
            ("n", "<C-g>", "/", "expected a slash command"),
            ("n", "<C-g>", "hello world", "expected a slash command"),
            ("n", "<C-g>", "/1abc", "expected a slash command"),
        ] {
            let err = KeymapSpec::parse(mode, lhs, command).expect_err(command);
            assert!(err.contains(expected), "{command}: {err}");
        }
    }
}

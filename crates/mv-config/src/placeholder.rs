//! `<<PLACEHOLDER: …>>` values: carried through as-is, never guessed.
//!
//! `config.example.toml` ships every required path and model id as a
//! placeholder string that will not resolve to anything usable
//! (`config.py:26-34`). Multiverse never invents a value for one: a
//! field holding a placeholder is carried verbatim into the loaded
//! [`crate::load::Config`], skipped by whatever validation an ordinary
//! value would get (absolute-path checks, directory-exists checks,
//! …), and left for `multiverse check` or a dispatch dry-run to name
//! so a human can fill it in.

/// Marks the start of a placeholder sentinel, e.g.
/// `<<PLACEHOLDER: absolute path to the personal Claude config dir>>`.
const PREFIX: &str = "<<PLACEHOLDER:";
const SUFFIX: &str = ">>";

/// Whether `value` is a placeholder sentinel rather than a real value.
///
/// A sentinel is recognized by shape alone (`<<PLACEHOLDER: ...>>`),
/// not by contents — this module never tries to tell a "real but
/// unlikely" value from a placeholder a human forgot to fill in.
pub fn is_placeholder(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.starts_with(PREFIX)
        && trimmed.ends_with(SUFFIX)
        && trimmed.len() >= PREFIX.len() + SUFFIX.len()
}

/// A string field that is either a real value or an unfilled
/// placeholder, carried verbatim either way.
///
/// The point of this type is not to parse anything out of a
/// placeholder — there is nothing to parse — but to give every caller
/// one place to ask "is this real yet?" instead of re-deriving the
/// sentinel check, and to make "skip validation for a placeholder"
/// the obvious thing to write rather than an easy thing to forget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaybePlaceholder {
    /// A real, filled-in value.
    Value(String),
    /// The placeholder string exactly as written in the TOML file.
    Placeholder(String),
}

impl MaybePlaceholder {
    /// Classify `raw` as a real value or a placeholder, carrying the
    /// original text through unchanged either way.
    pub fn parse(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        if is_placeholder(&raw) {
            Self::Placeholder(raw)
        } else {
            Self::Value(raw)
        }
    }

    pub fn is_placeholder(&self) -> bool {
        matches!(self, Self::Placeholder(_))
    }

    /// The real value, or `None` while this is still a placeholder.
    pub fn value(&self) -> Option<&str> {
        match self {
            Self::Value(v) => Some(v),
            Self::Placeholder(_) => None,
        }
    }

    /// The raw string as it appears in the TOML file, placeholder or
    /// not — what a dry-run or `check` prints back to the human.
    pub fn as_raw(&self) -> &str {
        match self {
            Self::Value(v) | Self::Placeholder(v) => v,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_a_well_formed_placeholder() {
        assert!(is_placeholder(
            "<<PLACEHOLDER: absolute path to the personal Claude config dir>>"
        ));
        assert!(is_placeholder("  <<PLACEHOLDER: x>>  "));
    }

    #[test]
    fn does_not_flag_an_ordinary_value() {
        assert!(!is_placeholder("/home/user/.config/claude"));
        assert!(!is_placeholder(""));
        assert!(!is_placeholder("<<not a placeholder"));
        assert!(!is_placeholder("not a placeholder>>"));
    }

    #[test]
    fn parse_carries_a_real_value_through() {
        let v = MaybePlaceholder::parse("/abs/path");
        assert!(!v.is_placeholder());
        assert_eq!(v.value(), Some("/abs/path"));
        assert_eq!(v.as_raw(), "/abs/path");
    }

    #[test]
    fn parse_carries_a_placeholder_through_without_guessing() {
        let raw = "<<PLACEHOLDER: absolute path to the work Claude config dir>>";
        let v = MaybePlaceholder::parse(raw);
        assert!(v.is_placeholder());
        assert_eq!(v.value(), None);
        assert_eq!(v.as_raw(), raw);
    }
}

//! `toml_edit`-based in-place edits: `add`/`new` append, never
//! rewrite, and preserve the user's comments.
//!
//! `cli.py:6429-6432`'s `add`/`new` commands append a new
//! `[accounts.<name>]` or `[projects.<name>]` block to the end of its
//! parent table without touching anything already there. `toml_edit`'s
//! `DocumentMut` is what makes that possible: unlike `toml::Value`, it
//! keeps every comment and the original formatting of everything it
//! does not touch, so round-tripping a hand-commented `config.toml`
//! through this module changes only the lines this module adds.

use toml_edit::{DocumentMut, Item, Table, Value};

use crate::error::ConfigError;

/// Appends a new `[accounts.<name>]` block with the given scalar
/// string fields, in document order, after everything already in the
/// `[accounts]` table. Refuses if an account by that name already
/// exists, or if the document's top-level `accounts` key is present
/// but is not itself a table.
pub fn append_account(
    doc: &mut DocumentMut,
    name: &str,
    fields: &[(&str, &str)],
) -> Result<(), ConfigError> {
    append_subtable(doc, "accounts", name, fields)
}

/// Appends a new `[projects.<name>]` block the same way
/// [`append_account`] appends an account block.
pub fn append_project(
    doc: &mut DocumentMut,
    name: &str,
    fields: &[(&str, &str)],
) -> Result<(), ConfigError> {
    append_subtable(doc, "projects", name, fields)
}

fn append_subtable(
    doc: &mut DocumentMut,
    parent_key: &str,
    child_key: &str,
    fields: &[(&str, &str)],
) -> Result<(), ConfigError> {
    let root = doc.as_table_mut();
    let parent_item = root
        .entry(parent_key)
        .or_insert_with(|| Item::Table(Table::new()));
    let parent = parent_item
        .as_table_mut()
        .ok_or_else(|| ConfigError::new(format!("{parent_key} is not a table in this config")))?;

    if parent.contains_key(child_key) {
        return Err(ConfigError::new(format!(
            "{parent_key}.{child_key} already exists — append never overwrites"
        )));
    }

    let mut child = Table::new();
    child.set_implicit(false);
    for (key, value) in fields {
        child.insert(key, Item::Value(Value::from(*value)));
    }
    parent.insert(child_key, Item::Table(child));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMENTED_CONFIG: &str = r#"# Herdr Multiverse — example configuration
max_concurrent = 3

[accounts.claude-personal]
# A seat-level comment that must survive untouched.
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: absolute path to the personal Claude config dir>>"
"#;

    #[test]
    fn append_account_preserves_every_existing_comment() {
        let mut doc: DocumentMut = COMMENTED_CONFIG.parse().unwrap();
        append_account(
            &mut doc,
            "claude-work",
            &[
                ("kind", "claude"),
                ("config_dir_var", "CLAUDE_CONFIG_DIR"),
                (
                    "config_dir",
                    "<<PLACEHOLDER: absolute path to the work Claude config dir>>",
                ),
            ],
        )
        .unwrap();

        let rendered = doc.to_string();
        assert!(rendered.contains("# Herdr Multiverse — example configuration"));
        assert!(rendered.contains("# A seat-level comment that must survive untouched."));
        assert!(rendered.contains("[accounts.claude-personal]"));
        assert!(rendered.contains("[accounts.claude-work]"));
        // The original block, byte for byte, is still a prefix-shaped
        // substring of the output: nothing about it was rewritten.
        assert!(rendered.contains(COMMENTED_CONFIG.trim_end()));
    }

    #[test]
    fn append_account_refuses_to_clobber_an_existing_one() {
        let mut doc: DocumentMut = COMMENTED_CONFIG.parse().unwrap();
        let err = append_account(&mut doc, "claude-personal", &[("kind", "claude")]).unwrap_err();
        assert!(err.message().contains("already exists"));
    }

    #[test]
    fn append_project_creates_the_parent_table_when_absent() {
        let mut doc: DocumentMut = "max_concurrent = 3\n".parse().unwrap();
        append_project(&mut doc, "example", &[("path", "/abs/path")]).unwrap();
        let rendered = doc.to_string();
        assert!(rendered.contains("[projects.example]"));
        assert!(rendered.contains("path = \"/abs/path\""));
    }
}

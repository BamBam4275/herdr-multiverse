//! A finding is a record that failed to parse: something a Rust reader
//! would need to reject, mis-handle, or crash on. A variant is the
//! opposite — an input shape a *tolerant* reader must accept — counted so
//! `result.md` can report real coverage instead of a bare pass/fail.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Finding {
    pub file: PathBuf,
    pub line: Option<usize>,
    pub message: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}:{}: {}", self.file.display(), line, self.message),
            None => write!(f, "{}: {}", self.file.display(), self.message),
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub variants: BTreeMap<String, usize>,
}

impl Report {
    pub fn fail(&mut self, file: &Path, line: Option<usize>, message: impl Into<String>) {
        self.findings.push(Finding {
            file: file.to_path_buf(),
            line,
            message: message.into(),
        });
    }

    pub fn note(&mut self, variant: &str) {
        *self.variants.entry(variant.to_string()).or_insert(0) += 1;
    }
}

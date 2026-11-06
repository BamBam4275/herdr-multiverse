//! Embed `prompts/` and `templates/` without hardcoding filenames.
//!
//! Another lane is renaming role prompt files as the cutover lands, so this
//! crate walks the directories at build time and `include_str!`s whatever is
//! there. A missing directory fails the build; a missing role file is a
//! runtime [`MissingPromptError`] looked up by `role-{key}.md`.

use std::env;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

fn main() {
    let manifest =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let repo = manifest.join("../..");
    let prompts = repo.join("prompts");
    let templates = repo.join("templates");
    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));

    println!("cargo:rerun-if-changed={}", prompts.display());
    println!("cargo:rerun-if-changed={}", templates.display());

    write_map(&out.join("embedded_prompts.rs"), &walk(&prompts, "prompts"));
    write_map(
        &out.join("embedded_templates.rs"),
        &walk(&templates, "templates"),
    );
}

struct EmbeddedFile {
    relative: String,
    absolute: PathBuf,
}

fn walk(root: &Path, label: &str) -> Vec<EmbeddedFile> {
    if !root.is_dir() {
        panic!(
            "mv-engine build.rs: {label}/ is missing at {} — the binary embeds that directory",
            root.display()
        );
    }
    let mut files = Vec::new();
    walk_into(root, "", &mut files);
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    if files.is_empty() {
        panic!("mv-engine build.rs: {label}/ contained no files to embed");
    }
    files
}

fn walk_into(dir: &Path, prefix: &str, files: &mut Vec<EmbeddedFile>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("reading {}: {err}", dir.display()))
        .map(|entry| entry.unwrap_or_else(|err| panic!("dir entry: {err}")))
        .collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let file_type = entry
            .file_type()
            .unwrap_or_else(|err| panic!("file type of {}: {err}", path.display()));
        if file_type.is_symlink() {
            continue;
        }
        let relative = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if file_type.is_dir() {
            walk_into(&path, &relative, files);
        } else if file_type.is_file() {
            println!("cargo:rerun-if-changed={}", path.display());
            let absolute = path.canonicalize().unwrap_or_else(|err| {
                panic!("canonicalize {}: {err}", path.display());
            });
            files.push(EmbeddedFile { relative, absolute });
        }
    }
}

fn write_map(path: &Path, files: &[EmbeddedFile]) {
    let mut out = BufWriter::new(File::create(path).expect("create generated embed map"));
    writeln!(out, "{{").expect("write generated embed map");
    writeln!(out, "    match relative {{").expect("write generated embed map");
    for file in files {
        let abs = file.absolute.to_str().unwrap_or_else(|| {
            panic!("embedded path is not UTF-8: {}", file.absolute.display());
        });
        let abs = abs.replace('\\', "\\\\").replace('"', "\\\"");
        writeln!(
            out,
            "        {:?} => Some(include_str!(\"{abs}\")),",
            file.relative
        )
        .expect("write generated embed map");
    }
    writeln!(out, "        _ => None,").expect("write generated embed map");
    writeln!(out, "    }}").expect("write generated embed map");
    writeln!(out, "}}").expect("write generated embed map");
    out.flush().expect("flush generated embed map");
}

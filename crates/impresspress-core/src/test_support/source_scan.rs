//! One walk over this crate's Rust sources, and one place that decides what
//! counts as a comment.
//!
//! Several gates in this crate are source scans: they assert that a shape
//! nothing in the type system can refuse — a raw table name, an unpaged read,
//! a hand-written error mapping, a config key spelled as a literal — appears
//! only where it is allowed to. Each of them needs the same two things: every
//! `.rs` file under a root, and a view of that file with its prose removed so
//! a gate's own explanation of what it bans is not itself a violation.
//!
//! Those two things were written out four times, once per gate, with four
//! slightly different sets of bugs available to them. This module is the
//! single copy. A gate states its root and its exemptions and gets back
//! [`SourceFile`]s; it picks a comment policy from the two below.
//!
//! ## Why there are two comment policies and not one
//!
//! They are different rules, not two spellings of one rule.
//!
//! [`strip_line_comments`] drops a line that is *entirely* a comment and keeps
//! a trailing comment on a line of code. That is what a gate wants when the
//! thing it bans is a token that could be hidden behind a `//` on the same
//! line as the code using it — the trailing comment stays in the haystack, so
//! nothing hides there.
//!
//! [`code_before_comment`] cuts each line at its first `//`, keeping only the
//! code. That is what a gate wants when it matches a *call*: `foo(` inside a
//! trailing comment is prose about the call, not the call, and a gate that
//! counted it would fail on every line of documentation that names the
//! function it bans.
//!
//! Neither is a Rust parser. A `//` inside a string literal ends the line for
//! [`code_before_comment`], and a block comment (`/* .. */`) is invisible to
//! both. Every gate here matches tokens that would be flagged, at worst,
//! one line too few — stated so the limit is written down rather than
//! implied.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// One `.rs` file the walk reached.
pub struct SourceFile {
    /// Path relative to the walk's root, with `/` separators — what an
    /// allowlist entry and a failure message name.
    pub rel: String,
    /// The full path on disk, for a message that has to be openable.
    pub path: PathBuf,
    /// The file's contents, read once.
    pub text: String,
}

/// Every `.rs` file under a root, minus the directories and file names a gate
/// exempts.
///
/// Built with [`Self::crate_src`] (or [`Self::new`] for a subtree or, in a
/// gate's own self-test, a temporary one), narrowed with [`Self::skip_dir`] /
/// [`Self::skip_file`], and closed with [`Self::least`] — which is what keeps
/// a gate from passing because the walk reached nothing.
pub struct SourceWalk {
    root: PathBuf,
    skip_dirs: Vec<&'static str>,
    skip_files: Vec<&'static str>,
    least: usize,
}

impl SourceWalk {
    /// A walk rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            skip_dirs: Vec::new(),
            skip_files: Vec::new(),
            least: 0,
        }
    }

    /// A walk over this crate's whole `src/` tree.
    ///
    /// `CARGO_MANIFEST_DIR` is `impresspress-core` both here and in the
    /// `tests/` integration crates, which are separate compilation units of
    /// the same package — so an integration gate and a `#[cfg(test)]` one see
    /// the same tree.
    pub fn crate_src() -> Self {
        Self::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))
    }

    /// Skip every directory with this name, at any depth, and everything
    /// under it.
    pub fn skip_dir(mut self, name: &'static str) -> Self {
        self.skip_dirs.push(name);
        self
    }

    /// Skip every file with this name, at any depth.
    pub fn skip_file(mut self, name: &'static str) -> Self {
        self.skip_files.push(name);
        self
    }

    /// Panic unless the walk reaches at least `count` files.
    ///
    /// A source gate that scans nothing passes for exactly the same reason a
    /// clean codebase does. This is the floor that tells the two apart when a
    /// root moves or a filter goes wrong; it is deliberately not set on the
    /// temporary trees a gate's own self-test plants.
    pub fn least(mut self, count: usize) -> Self {
        self.least = count;
        self
    }

    /// Read every file the walk reaches, sorted by [`SourceFile::rel`].
    pub fn collect(&self) -> Vec<SourceFile> {
        let mut out = Vec::new();
        self.visit(&self.root, &mut out);
        out.sort_by(|a, b| a.rel.cmp(&b.rel));
        assert!(
            out.len() >= self.least,
            "the walk over {} reached {} files, fewer than the {} it claims to \
             scan; a gate built on it would pass on an empty result",
            self.root.display(),
            out.len(),
            self.least
        );
        out
    }

    fn visit(&self, dir: &Path, out: &mut Vec<SourceFile>) {
        for entry in fs::read_dir(dir).expect("read source dir") {
            let path = entry.expect("dir entry").path();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if path.is_dir() {
                if !self.skip_dirs.contains(&name.as_str()) {
                    self.visit(&path, out);
                }
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs")
                || self.skip_files.contains(&name.as_str())
            {
                continue;
            }
            let rel = path
                .strip_prefix(&self.root)
                .expect("under root")
                .to_string_lossy()
                .replace('\\', "/");
            let text = fs::read_to_string(&path).expect("read source file");
            out.push(SourceFile { rel, path, text });
        }
    }
}

/// `src` without its full-line comments (`//`, `///`, `//!`). A trailing
/// comment on a line of code stays, so nothing hides behind a `//` on the
/// same line as the code it describes.
pub fn strip_line_comments(src: &str) -> String {
    src.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The code part of one line: everything before its first `//`.
pub fn code_before_comment(line: &str) -> &str {
    line.split("//").next().unwrap_or(line)
}

/// `src` up to its first `#[cfg(test)]` attribute.
///
/// A test asserting on a shape is not a handler producing it. Truncating —
/// rather than tracking braces — is the honest version: the attribute is
/// always the last thing in these files, and a gate that guessed at nesting
/// would be a second, worse Rust parser.
pub fn strip_test_modules(src: &str) -> String {
    src.lines()
        .take_while(|line| !line.trim_start().starts_with("#[cfg(test)]"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_line_comments_drops_full_line_comments_and_keeps_trailing_ones() {
        let src = "//! doc\nlet a = 1; // trailing\n/// more\n  // indented\nlet b = 2;\n";
        assert_eq!(
            strip_line_comments(src),
            "let a = 1; // trailing\nlet b = 2;"
        );
    }

    #[test]
    fn code_before_comment_keeps_only_the_code() {
        assert_eq!(code_before_comment("let a = 1; // note"), "let a = 1; ");
        assert_eq!(code_before_comment("// all of it"), "");
        assert_eq!(code_before_comment("let a = 1;"), "let a = 1;");
    }

    #[test]
    fn strip_test_modules_cuts_at_the_attribute() {
        let src = "pub fn f() {}\n\n#[cfg(test)]\nmod tests {\n    fn g() {}\n}\n";
        assert_eq!(strip_test_modules(src), "pub fn f() {}\n");
    }

    /// The walk descends, honours both exemptions, and reads only Rust.
    #[test]
    fn the_walk_descends_and_honours_its_exemptions() {
        let root = std::env::temp_dir().join(format!("source-walk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("nested/tests")).expect("temp tree");
        fs::write(root.join("top.rs"), "// top\n").expect("top");
        fs::write(root.join("nested/deep.rs"), "// deep\n").expect("deep");
        fs::write(root.join("nested/tests/fixture.rs"), "// exempt\n").expect("exempt");
        fs::write(root.join("nested/skipped.rs"), "// skipped\n").expect("skipped");
        fs::write(root.join("notes.txt"), "not rust\n").expect("non-rust");

        let found = SourceWalk::new(&root)
            .skip_dir("tests")
            .skip_file("skipped.rs")
            .collect();
        fs::remove_dir_all(&root).expect("clean up");

        let rels: Vec<&str> = found.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, ["nested/deep.rs", "top.rs"]);
    }

    /// The floor fails rather than reporting an empty scan as clean.
    #[test]
    fn the_floor_refuses_a_walk_that_reached_nothing() {
        let root = std::env::temp_dir().join(format!("source-walk-floor-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp tree");
        let walk = SourceWalk::new(&root).least(1);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| walk.collect()));
        fs::remove_dir_all(&root).expect("clean up");
        assert!(
            result.is_err(),
            "a walk that reached no file reported a clean scan"
        );
    }
}

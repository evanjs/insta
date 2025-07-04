use std::error::Error;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use insta::_cargo_insta_support::TextSnapshotContents;
use proc_macro2::{LineColumn, TokenTree};

use syn::__private::ToTokens;
use syn::spanned::Spanned;

#[derive(Debug, Clone)]
struct InlineSnapshot {
    start: (usize, usize),
    end: (usize, usize),
    indentation: String,
}

#[derive(Clone)]
pub(crate) struct FilePatcher {
    filename: PathBuf,
    lines: Vec<String>,
    source: syn::File,
    inline_snapshots: Vec<InlineSnapshot>,
}

impl fmt::Debug for FilePatcher {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("FilePatcher")
            .field("filename", &self.filename)
            .field("inline_snapshots", &self.inline_snapshots)
            .finish()
    }
}

impl FilePatcher {
    pub(crate) fn open(p: &Path) -> Result<FilePatcher, Box<dyn Error>> {
        let filename = p.to_path_buf();
        let contents = fs::read_to_string(p)?;
        let source = syn::parse_file(&contents)?;
        let lines: Vec<String> = contents.lines().map(|x| x.into()).collect();
        Ok(FilePatcher {
            filename,
            source,
            lines,
            inline_snapshots: vec![],
        })
    }

    pub(crate) fn save(&self) -> Result<(), Box<dyn Error>> {
        // We use a temp file and then atomically rename to prevent a
        // file watcher restarting the process midway through the write.
        let mut temp_file = tempfile::Builder::new()
            .suffix(".snap.tmp")
            .tempfile_in(self.filename.parent().ok_or("Parent directory not found")?)?;

        for line in &self.lines {
            writeln!(temp_file, "{line}")?;
        }

        temp_file.flush()?;
        fs::rename(temp_file.path(), &self.filename)?;

        Ok(())
    }

    pub(crate) fn add_snapshot_macro(&mut self, line: usize) -> bool {
        match self.find_snapshot_macro(line) {
            Some(snapshot) => {
                // this can happen if multiple snapshots were added in one
                // iteration of a loop.  In that case we want to ignore the
                // duplicate
                //
                // See https://github.com/mitsuhiko/insta/issues/340
                if self
                    .inline_snapshots
                    .last()
                    // x.end.0 is 0-origin whereas line is 1-origin
                    .map_or(false, |x| x.end.0 >= line - 1)
                {
                    return false;
                }
                self.inline_snapshots.push(snapshot);
                true
            }
            None => false,
        }
    }

    pub(crate) fn get_new_line(&self, id: usize) -> usize {
        self.inline_snapshots[id].start.0 + 1
    }

    pub(crate) fn set_new_content(&mut self, id: usize, snapshot: &TextSnapshotContents) {
        let inline = &mut self.inline_snapshots[id];

        // find prefix and suffix on the first and last lines
        let prefix: String = self.lines[inline.start.0]
            .chars()
            .take(inline.start.1)
            .collect();
        let suffix: String = self.lines[inline.end.0]
            .chars()
            .skip(inline.end.1)
            .collect();

        // replace lines
        let snapshot_line_contents =
            [prefix, snapshot.to_inline(&inline.indentation), suffix].join("");

        self.lines.splice(
            inline.start.0..=inline.end.0,
            snapshot_line_contents.lines().map(|l| l.to_string()),
        );

        // update other snapshot locations
        let old_lines_count = inline.end.0 - inline.start.0 + 1;
        let line_count_diff =
            (snapshot_line_contents.lines().count() as isize) - (old_lines_count as isize);
        for inl in &mut self.inline_snapshots[id..] {
            inl.start.0 = ((inl.start.0 as isize) + line_count_diff) as usize;
            inl.end.0 = ((inl.end.0 as isize) + line_count_diff) as usize;
        }
    }

    fn find_snapshot_macro(&self, line: usize) -> Option<InlineSnapshot> {
        struct Visitor<'a>(usize, Option<InlineSnapshot>, &'a [String]);

        fn indentation(macro_start: LineColumn, code_lines: &[String]) -> String {
            code_lines[macro_start.line - 1][..macro_start.column].to_owned()
        }

        fn scan_for_path_start(tokens: &[TokenTree], pos: usize, code_lines: &[String]) -> String {
            let mut rev_tokens = tokens[..=pos].iter().rev();
            let mut start = rev_tokens.next().unwrap();
            loop {
                if let Some(TokenTree::Punct(ref punct)) = rev_tokens.next() {
                    if punct.as_char() == ':' {
                        if let Some(TokenTree::Punct(ref punct)) = rev_tokens.next() {
                            if punct.as_char() == ':' {
                                if let Some(ident @ TokenTree::Ident(_)) = rev_tokens.next() {
                                    start = ident;
                                    continue;
                                }
                            }
                        }
                    }
                }
                break;
            }
            indentation(start.span().start(), code_lines)
        }

        impl Visitor<'_> {
            fn scan_nested_macros(&mut self, tokens: &[TokenTree]) {
                for idx in 0..tokens.len() {
                    // Look for the start of a macro (potential snapshot location)
                    if let Some(TokenTree::Ident(_)) = tokens.get(idx) {
                        if let Some(TokenTree::Punct(ref punct)) = tokens.get(idx + 1) {
                            if punct.as_char() == '!' {
                                if let Some(TokenTree::Group(ref group)) = tokens.get(idx + 2) {
                                    // Found a macro, determine its indentation
                                    let indentation = scan_for_path_start(tokens, idx, self.2);
                                    // Extract tokens from the macro arguments
                                    let tokens: Vec<_> = group.stream().into_iter().collect();
                                    // Try to extract a snapshot, passing the calculated indentation
                                    self.try_extract_snapshot(&tokens, indentation);
                                }
                            }
                        }
                    }
                }
                for token in tokens {
                    // recurse into groups
                    if let TokenTree::Group(group) = token {
                        let tokens: Vec<_> = group.stream().into_iter().collect();
                        self.scan_nested_macros(&tokens);
                    }
                }
            }

            fn try_extract_snapshot(&mut self, tokens: &[TokenTree], indentation: String) -> bool {
                // ignore optional trailing comma
                let tokens = match tokens.last() {
                    Some(TokenTree::Punct(ref punct)) if punct.as_char() == ',' => {
                        &tokens[..tokens.len() - 1]
                    }
                    _ => tokens,
                };

                if tokens.len() < 2 {
                    return false;
                }

                match tokens[tokens.len() - 2] {
                    TokenTree::Punct(ref punct) if punct.as_char() == '@' => {}
                    _ => {
                        return false;
                    }
                }

                let (start, end) = match &tokens[tokens.len() - 1] {
                    TokenTree::Literal(lit) => {
                        let span = lit.span();
                        (
                            (span.start().line - 1, span.start().column),
                            (span.end().line - 1, span.end().column),
                        )
                    }
                    _ => return false,
                };

                self.1 = Some(InlineSnapshot {
                    start,
                    end,
                    indentation,
                });
                true
            }
        }

        impl<'ast> syn::visit::Visit<'ast> for Visitor<'_> {
            fn visit_attribute(&mut self, i: &'ast syn::Attribute) {
                let start = i.span().start().line;
                let end = i.span().end().line;

                if start > self.0 || end < self.0 || i.path().segments.is_empty() {
                    return;
                }

                let tokens: Vec<_> = i.meta.to_token_stream().into_iter().collect();
                if !tokens.is_empty() {
                    self.scan_nested_macros(&tokens);
                }
            }
            fn visit_macro(&mut self, i: &'ast syn::Macro) {
                let span_start = i.span().start();
                let start = span_start.line;
                let end = i
                    .tokens
                    .clone()
                    .into_iter()
                    .last()
                    .map_or(start, |t| t.span().end().line);

                if start > self.0 || end < self.0 || i.path.segments.is_empty() {
                    return;
                }

                // if we have under two tokens there is not much else we need to do
                let tokens: Vec<_> = i.tokens.clone().into_iter().collect();
                if tokens.len() < 2 {
                    return;
                }

                // recurse into block-like macro such as allow_duplicates! { .. }
                if matches!(i.delimiter, syn::MacroDelimiter::Brace(_)) {
                    if let Ok(stmts) = i.parse_body_with(syn::Block::parse_within) {
                        for stmt in &stmts {
                            self.visit_stmt(stmt);
                        }
                        return;
                    }
                    // TODO: perhaps, we can return here and remove fallback to
                    // self.scan_nested_macros(&tokens)
                }

                let indentation = indentation(span_start, self.2);
                if !self.try_extract_snapshot(&tokens, indentation) {
                    // if we can't extract a snapshot here we want to scan for nested
                    // macros.  These are just represented as unparsed tokens in a
                    // token stream.
                    self.scan_nested_macros(&tokens);
                }
            }
        }

        let mut visitor = Visitor(line, None, &self.lines);
        syn::visit::visit_file(&mut visitor, &self.source);
        visitor.1
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_debug_snapshot;

    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_find_snapshot_macro() {
        let content = r######"
use insta::assert_snapshot;

fn test_function() {
    assert_snapshot!("test\ntest", @r###"
    test
    test
    "###);
}
"######;

        let file_patcher = FilePatcher {
            filename: PathBuf::new(),
            lines: content.lines().map(String::from).collect(),
            source: syn::parse_file(content).unwrap(),
            inline_snapshots: vec![],
        };

        // The snapshot macro starts on line 5 (1-based index)
        let snapshot = file_patcher.find_snapshot_macro(5).unwrap();

        // Extract the snapshot content
        let snapshot_content: Vec<String> =
            file_patcher.lines[snapshot.start.0..=snapshot.end.0].to_vec();

        assert_debug_snapshot!(snapshot_content, @r####"
        [
            "    assert_snapshot!(\"test\\ntest\", @r###\"",
            "    test",
            "    test",
            "    \"###);",
        ]
        "####);

        // Assert the indentation
        assert_debug_snapshot!(snapshot.indentation, @r#""    ""#);
    }

    #[test]
    fn test_find_snapshot_macro_with_tabs() {
        let content = r######"
use insta::assert_snapshot;

fn test_function() {
	assert_snapshot!("test\ntest", @r###"
	test
	test
	"###);
	// visitor shouldn't panic because of macro at column > start_line_len
	                                       assert_snapshot!("", @"");
}
"######;

        let file_patcher = FilePatcher {
            filename: PathBuf::new(),
            lines: content.lines().map(String::from).collect(),
            source: syn::parse_file(content).unwrap(),
            inline_snapshots: vec![],
        };

        // The snapshot macro starts on line 5 (1-based index)
        let snapshot = file_patcher.find_snapshot_macro(5).unwrap();

        // Extract the snapshot content
        let snapshot_content: Vec<String> =
            file_patcher.lines[snapshot.start.0..=snapshot.end.0].to_vec();

        assert_debug_snapshot!(snapshot_content, @r####"
        [
            "\tassert_snapshot!(\"test\\ntest\", @r###\"",
            "\ttest",
            "\ttest",
            "\t\"###);",
        ]
        "####);

        // Assert the indentation
        assert_debug_snapshot!(snapshot.indentation, @r#""\t""#);
    }

    #[test]
    fn test_find_snapshot_macro_within_allow_duplicates() {
        let content = r######"
fn test_function() {
    insta::allow_duplicates! {
        for x in 0..10 {
            insta::assert_snapshot!("foo", @"foo"); // 5
            insta::assert_snapshot!("bar", @"bar"); // 6
        }
    }
}
"######;

        let file_patcher = FilePatcher {
            filename: PathBuf::new(),
            lines: content.lines().map(String::from).collect(),
            source: syn::parse_file(content).unwrap(),
            inline_snapshots: vec![],
        };

        let snapshot5 = file_patcher.find_snapshot_macro(5).unwrap();
        let snapshot6 = file_patcher.find_snapshot_macro(6).unwrap();

        // Extract the snapshot contents
        let snapshot_content5 = file_patcher.lines[snapshot5.start.0..=snapshot5.end.0].to_vec();
        let snapshot_content6 = file_patcher.lines[snapshot6.start.0..=snapshot6.end.0].to_vec();

        assert_debug_snapshot!(snapshot_content5, @r#"
        [
            "            insta::assert_snapshot!(\"foo\", @\"foo\"); // 5",
        ]
        "#);
        assert_debug_snapshot!(snapshot_content6, @r#"
        [
            "            insta::assert_snapshot!(\"bar\", @\"bar\"); // 6",
        ]
        "#);

        // Assert the indentation
        assert_debug_snapshot!(snapshot5.indentation, @r#""            ""#);
        assert_debug_snapshot!(snapshot6.indentation, @r#""            ""#);
    }
}

//! This module tokenises Rust code and looks for the unsafe keyword. This is done as an additional
//! layer of defence in addition to use of the -Funsafe-code flag when compiling crates, since that
//! flag unfortunately doesn't completely prevent use of unsafe.

use crate::location::SourceLocation;
use anyhow::Context;
use anyhow::Result;
use ra_ap_rustc_lexer::Token;
use ra_ap_rustc_lexer::TokenKind;
use cargo_metadata::Source;
use std::fs::File;
use std::io::Write;
use std::path::Path;

use std::fs::read_to_string;
use serde::{Deserialize, Serialize};
use serde_json::from_str;

#[derive(Serialize, Deserialize)]
pub struct UnsafeBlock {
    pub filename: String,
    pub signature: String,
    pub body: String,
}

/// Returns the locations of all unsafe usages found in `path`, except if followed by extern
pub(crate) fn scan_path(path: &Path) -> Result<Vec<SourceLocation>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read `{}`", path.display()))?;
    let Ok(source) = std::str::from_utf8(&bytes) else {
        // If the file isn't valid UTF-8 then we don't need to check it for the unsafe keyword,
        // since it can't be a source file that the rust compiler would accept.
        return Ok(Vec::new());
    };
    Ok(scan_string(source, path))
}

fn scan_string(source: &str, path: &Path) -> Vec<SourceLocation> {
    let mut token_start_offset = 0;
    let mut locations = Vec::new();

    let skip_condition = |x| {
        matches!(
            x,
            TokenKind::BlockComment {
                doc_style: _,
                terminated: _
            } | TokenKind::LineComment { doc_style: _ }
                | TokenKind::Whitespace
        )
    };

    let mut iter = ra_ap_rustc_lexer::tokenize(source, ra_ap_rustc_lexer::FrontmatterAllowed::No);
    let mut previous_token_text = "";
    let mut previous_token_end_offset = 0;
    let mut previous_token_length = 0;

    while let (Some(token), skipped_offset) = next_token(&mut iter, skip_condition) {
        let token_length = usize::try_from(token.len).unwrap();

        token_start_offset += skipped_offset;

        let token_end_offset = token_start_offset + token_length;
        let token_text = &source[token_start_offset..token_end_offset];
        if !previous_token_text.is_empty() {
            // check against previous token
            if check_tokens(previous_token_text, token_text) {
                add_location(
                    source,
                    path,
                    &mut locations,
                    previous_token_end_offset,
                    previous_token_length,
                );
            }
        }
        previous_token_text = token_text;
        previous_token_end_offset = token_end_offset;
        previous_token_length = token_length;
        // always check against potential future token
        if let (Some(next_token), skipped_offset) = next_token(&mut iter, skip_condition) {
            // the next token starts after the end of the current token
            token_start_offset += token_length;
            token_start_offset += skipped_offset;

            let next_token_length = usize::try_from(next_token.len).unwrap();

            let next_token_end_offset = token_start_offset + next_token_length;
            let next_token_text = &source[token_start_offset..next_token_end_offset];
            if check_tokens(token_text, next_token_text) {
                add_location(source, path, &mut locations, token_end_offset, token_length);
            }
            token_start_offset = next_token_end_offset;
            // as we consumed the next token already, this will be our previous token in the next loop iteration
            previous_token_text = next_token_text;
            previous_token_end_offset = next_token_end_offset;
            previous_token_length = next_token_length;
        } else {
            // current token is last token in file
            // this should never be valid code, but we flag it nevertheless to be safe
            if token_text == "unsafe" {
                add_location(source, path, &mut locations, token_end_offset, token_length);
            }
            // as there should not be another loop iteration this should be useless
            // we do it just for completeness
            token_start_offset = token_end_offset;
        }
    }
    locations
}

fn check_tokens(first_token_text: &str, second_token_text: &str) -> bool {
    first_token_text == "unsafe" && second_token_text != "extern"
}

/// Returns the next relevant token according to the condition given and the offset to it
fn next_token<F>(
    iter: &mut impl Iterator<Item = Token>,
    skip_condition: F,
) -> (Option<Token>, usize)
where
    F: Fn(TokenKind) -> bool,
{
    let mut skipped_offset = 0;

    for next in iter.by_ref() {
        if skip_condition(next.kind) {
            skipped_offset += usize::try_from(next.len).unwrap();
        } else {
            return (Some(next), skipped_offset);
        }
    }
    (None, skipped_offset)
}

fn add_location(
    source: &str,
    path: &Path,
    locations: &mut Vec<SourceLocation>,
    token_end_offset: usize,
    token_length: usize,
) {
    let column = source[..token_end_offset]
        .lines()
        .last()
        .map(|line| (line.len() - token_length + 1) as u32)
        .unwrap_or(1);
    let line = 1.max(source[..token_end_offset].lines().count() as u32);
    locations.push(SourceLocation::new(path, line, Some(column)));
}

pub(crate) fn filter_not_registered(unfiltered: Vec<SourceLocation>) -> Vec<SourceLocation>{
    let filename = "unsafe-blocks.json";
    let saved_unsafe_blocks = load_saved_unsafe_block(filename);

    let mut filtered = Vec::new();

    for location in unfiltered {
        let option = extract_specific_unsafe_block(
            location.filename().to_str().unwrap(), 
            location.line(), 
            location.column().unwrap()
        );

        if let Some(block) = option {
            if !is_registered(block, &saved_unsafe_blocks) {
                filtered.push(location);
            }
        }
        else {
            filtered.push(location);
        }
    }

    filtered
}

fn load_saved_unsafe_block(source_filename: &str) -> Vec<UnsafeBlock> {
    let saved_json = read_to_string(source_filename)
        .expect("Error when reading saved json");

    let saved_blocks: Vec<UnsafeBlock> = from_str(&saved_json).expect("Error when deserializing");

    saved_blocks
}

fn is_registered(sample: UnsafeBlock, registered_blocks: &Vec<UnsafeBlock>) -> bool{
    for block in registered_blocks {
        if sample.filename == block.filename && sample.signature == block.signature && sample.body == block.body {
            return true
        }
    }

    false
}

fn extract_specific_unsafe_block(filename: &str, ref_line: u32, ref_column: u32) -> Option<UnsafeBlock> {
    let source = read_to_string(filename).unwrap();

    let mut offset = 0;
    let mut is_unsafe_block = false;
    let mut counter: i8 = -1;

    let mut begin: usize = 0;
    let mut middle: usize = 0;

    for token in ra_ap_rustc_lexer::tokenize(&source, ra_ap_rustc_lexer::FrontmatterAllowed::No) {
        let new_offset = offset + usize::try_from(token.len).unwrap();
        let token_text = &source[offset..new_offset];

        if is_unsafe_block {
            if token_text == "{" {
                counter += 1;

                if counter == 0 {
                    middle = offset - 1;
                }
            }

            if token_text == "}" {
                counter -= 1;

                if counter <= 0 {
                    return Some(UnsafeBlock { filename: filename.to_string(), signature: source[begin..middle].to_string(), body: source[middle..new_offset].to_string() });
                }
            }
        }

        if token_text == "unsafe" {
            let column = source[..new_offset]
                .lines()
                .last()
                .map(|line| (line.len() - token_text.len() + 1) as u32)
                .unwrap_or(1);
            let line = 1.max(source[..new_offset].lines().count() as u32);

            if line == ref_line && column == ref_column {
                is_unsafe_block = true;
                counter = -1;

                begin = offset;
                middle = offset;
            }
        }

        offset = new_offset;
    }

    None
}

#[cfg(test)]
mod tests {
    use crate::unsafe_checker::scan_path;
    use crate::unsafe_checker::scan_string;
    use std::ops::Not;
    use std::path::Path;

    fn unsafe_line_col(source: &str) -> Option<(u32, u32)> {
        scan_string(source, Path::new("test.rs"))
            .first()
            .map(|usage| (usage.line(), usage.column().unwrap()))
    }

    #[test]
    fn test_scan_string() {
        assert_eq!(unsafe_line_col("unsafe fn foo() {}"), Some((1, 1)));
        assert_eq!(
            unsafe_line_col(r#"fn foo() -> &'static str {"unsafe"}"#),
            None
        );
        assert_eq!(unsafe_line_col("fn foo() { unsafe {} }"), Some((1, 12)));
        assert_eq!(
            unsafe_line_col(indoc::indoc! {r#"
                fn foo() {
                    unsafe {}
                }"#
            }),
            Some((2, 5))
        );
        assert_eq!(
            unsafe_line_col("#[cfg(foo)]\nunsafe fn bar() {}"),
            Some((2, 1))
        );
    }

    #[track_caller]
    fn has_unsafe_in_file(path: &str) -> bool {
        let root = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR should be set");
        let root = Path::new(&root);
        scan_path(&root.join(path)).unwrap().is_empty().not()
    }

    #[test]
    fn test_scan_test_crates() {
        assert!(has_unsafe_in_file("test_crates/crab-1/src/lib.rs"));
        assert!(has_unsafe_in_file("test_crates/crab-1/src/impl1.rs"));
        assert!(!has_unsafe_in_file("test_crates/crab-2/src/lib.rs"));
        assert!(has_unsafe_in_file("test_crates/crab-3/src/lib.rs"));
        assert!(has_unsafe_in_file("test_crates/crab-bin/src/main.rs"));
    }
}

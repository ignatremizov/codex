//! Statement boundaries from the same JavaScript grammar used for syntax colors.
//!
//! Strings, regex literals, templates and comments stay opaque. Parenthesized expressions
//! stay together, including the semicolons in a C-style `for` header.

use super::find_syntax;
use super::syntax_set;
use syntect::easy::ScopeRangeIterator;
use syntect::parsing::ParseState;
use syntect::parsing::Scope;
use syntect::parsing::ScopeStack;
use syntect::util::LinesWithEndings;

pub(crate) fn statement_breaks(code: &str) -> Option<Vec<usize>> {
    let mut parser = ParseState::new(find_syntax("javascript")?);
    let mut stack = ScopeStack::new();
    let string = Scope::new("string").ok()?;
    let comment = Scope::new("comment").ok()?;
    let template = Scope::new("meta.string").ok()?;
    let interpolation = Scope::new("meta.interpolation").ok()?;
    let mut parentheses = 0usize;
    let mut offset = 0;
    let mut breaks = Vec::new();
    for line in LinesWithEndings::from(code) {
        let operations = parser.parse_line(line, syntax_set()).ok()?;
        for (range, operation) in ScopeRangeIterator::new(&operations, line) {
            stack.apply(operation).ok()?;
            if stack.as_slice().iter().any(|scope| {
                string.is_prefix_of(*scope)
                    || comment.is_prefix_of(*scope)
                    || template.is_prefix_of(*scope)
                    || interpolation.is_prefix_of(*scope)
            }) {
                continue;
            }
            for (index, byte) in line[range.clone()].bytes().enumerate() {
                match byte {
                    b'(' => parentheses += 1,
                    b')' => parentheses = parentheses.saturating_sub(/*rhs*/ 1),
                    b';' if parentheses == 0 => breaks.push(offset + range.start + index + 1),
                    _ => {}
                }
            }
        }
        offset += line.len();
    }
    Some(breaks)
}

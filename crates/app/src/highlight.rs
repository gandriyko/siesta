//! SQL highlighting: gpui-component's tree-sitter `sql` grammar, with `-- @directive` comments
//! painted as `attribute` instead of `comment` (the same role Respite gives its `# @directives`).
//!
//! The editor's own tree-sitter adapter is private, so this wraps the public `SyntaxHighlighter`.
// ponytail: full reparse (synchronous) per edit, no background parse; fine for the file sizes SQL
// scripts have. Switch to incremental edits if a 1 MB dump ever feels slow.

use std::ops::Range;

use gpui::{Context, HighlightStyle, SharedString, Window};
use gpui_component::{
    highlighter::SyntaxHighlighter,
    input::{EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter, Rope},
};

pub struct SqlHighlighter {
    inner: SyntaxHighlighter,
    directives: Vec<Range<usize>>,
}

impl Default for SqlHighlighter {
    fn default() -> Self {
        Self { inner: SyntaxHighlighter::new("sql"), directives: vec![] }
    }
}

impl InputHighlighter for SqlHighlighter {
    fn language(&self) -> SharedString {
        "sql".into()
    }

    fn update(&mut self, _: Option<InputEdit>, text: &Rope, _: bool, _: &mut Window, _: &mut Context<EditorState>) {
        self.inner.update(None, text, None);
        self.directives = directive_spans(&text.to_string());
    }

    /// Multi-cursor typing: only the final text matters.
    fn update_batch(&mut self, edits: &[(InputEdit, Rope)], folding: bool, window: &mut Window, cx: &mut Context<EditorState>) {
        if let Some((edit, text)) = edits.last() {
            self.update(Some(*edit), text, folding, window, cx);
        }
    }

    fn styles(&self, range: &Range<usize>, resolver: &dyn HighlightStyleResolver) -> Vec<(Range<usize>, HighlightStyle)> {
        let base = self.inner.styles(range, resolver);
        match resolver.style("attribute") {
            Some(style) => overlay(base, &self.directives, style),
            None => base,
        }
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        vec![]
    }
}

/// Byte ranges of the lines that are `-- @…` comments (indentation excluded).
fn directive_spans(src: &str) -> Vec<Range<usize>> {
    let mut out = vec![];
    let mut off = 0;
    for raw in src.split_inclusive('\n') {
        let line = raw.trim_end_matches(['\n', '\r']);
        let t = line.trim_start();
        if t.strip_prefix("--").is_some_and(|rest| rest.trim_start().starts_with('@')) {
            out.push(off + line.len() - t.len()..off + line.len());
        }
        off += raw.len();
    }
    out
}

/// `base` (ordered runs covering a range) with `style` painted over the sorted, disjoint `spans`.
fn overlay(base: Vec<(Range<usize>, HighlightStyle)>, spans: &[Range<usize>], style: HighlightStyle) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut out = Vec::with_capacity(base.len());
    for (r, s) in base {
        let mut pos = r.start;
        let first = spans.partition_point(|d| d.end <= r.start);
        for d in spans[first..].iter().take_while(|d| d.start < r.end) {
            let (ds, de) = (d.start.max(r.start), d.end.min(r.end));
            if ds > pos {
                out.push((pos..ds, s));
            }
            out.push((ds..de, style));
            pos = de;
        }
        if pos < r.end {
            out.push((pos..r.end, s));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directives_are_found() {
        let src = "-- plain\n  -- @name users\nSELECT 1; -- @name no\n--@confirm\r\n";
        let spans = directive_spans(src);
        let text: Vec<&str> = spans.iter().map(|r| &src[r.clone()]).collect();
        assert_eq!(text, ["-- @name users", "--@confirm"]);
    }

    #[test]
    fn overlay_splits_runs() {
        let (a, b) = (HighlightStyle::default(), HighlightStyle { font_weight: Some(gpui::FontWeight::BOLD), ..Default::default() });
        let out = overlay(vec![(0..10, a), (10..20, a)], &[3..5, 9..12], b);
        let ranges: Vec<_> = out.iter().map(|(r, s)| (r.clone(), *s == b)).collect();
        assert_eq!(
            ranges,
            [(0..3, false), (3..5, true), (5..9, false), (9..10, true), (10..12, true), (12..20, false)]
        );
    }
}

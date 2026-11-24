//! SQL highlighting: gpui-component's tree-sitter `sql` grammar, with `-- @directive` comments
//! painted as `attribute` instead of `comment` (the same role Respite gives its `# @directives`).
//! `.mongodb.js` files: the `javascript` grammar with `// @directives` the same way.
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
    language: &'static str,
    /// What starts a directive comment: `--`, or `//` in `.mongodb.js`.
    comment: &'static str,
    directives: Vec<Range<usize>>,
}

impl Default for SqlHighlighter {
    fn default() -> Self {
        Self::new("sql", "--")
    }
}

impl SqlHighlighter {
    pub fn new(language: &'static str, comment: &'static str) -> Self {
        Self { inner: SyntaxHighlighter::new(language), language, comment, directives: vec![] }
    }
}

impl InputHighlighter for SqlHighlighter {
    fn language(&self) -> SharedString {
        self.language.into()
    }

    fn update(&mut self, _: Option<InputEdit>, text: &Rope, _: bool, _: &mut Window, _: &mut Context<EditorState>) {
        self.inner.update(None, text, None);
        self.directives = directive_spans(&text.to_string(), self.comment);
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

/// `.es` highlighting: JSON bodies by tree-sitter `json` (run over the text with request and
/// comment lines blanked, so offsets stay), the request line's method and path, `#` / `//`
/// comments and `# @directives` on top.
pub struct EsHighlighter {
    inner: SyntaxHighlighter,
    /// Disjoint, sorted: (range, theme role).
    spans: Vec<(Range<usize>, &'static str)>,
}

impl Default for EsHighlighter {
    fn default() -> Self {
        Self { inner: SyntaxHighlighter::new("json"), spans: vec![] }
    }
}

impl InputHighlighter for EsHighlighter {
    fn language(&self) -> SharedString {
        "json".into()
    }

    fn update(&mut self, _: Option<InputEdit>, text: &Rope, _: bool, _: &mut Window, _: &mut Context<EditorState>) {
        let (masked, spans) = es_spans(&text.to_string());
        self.inner.update(None, &Rope::from(masked.as_str()), None);
        self.spans = spans;
    }

    fn update_batch(&mut self, edits: &[(InputEdit, Rope)], folding: bool, window: &mut Window, cx: &mut Context<EditorState>) {
        if let Some((edit, text)) = edits.last() {
            self.update(Some(*edit), text, folding, window, cx);
        }
    }

    fn styles(&self, range: &Range<usize>, resolver: &dyn HighlightStyleResolver) -> Vec<(Range<usize>, HighlightStyle)> {
        let mut out = self.inner.styles(range, resolver);
        for role in ["keyword", "string", "comment", "attribute"] {
            let spans: Vec<Range<usize>> = self.spans.iter().filter(|(_, r)| *r == role).map(|(s, _)| s.clone()).collect();
            if let Some(style) = resolver.style(role) {
                out = overlay(out, &spans, style);
            }
        }
        out
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        vec![]
    }
}

/// The text with request and comment lines turned into spaces (same byte offsets), and the spans
/// to paint over the JSON highlighting.
fn es_spans(src: &str) -> (String, Vec<(Range<usize>, &'static str)>) {
    const METHODS: [&str; 6] = ["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];
    let mut masked = String::with_capacity(src.len());
    let mut spans = vec![];
    let mut off = 0;
    for raw in src.split_inclusive('\n') {
        let line = raw.trim_end_matches(['\n', '\r']);
        let indent = line.len() - line.trim_start().len();
        let t = line.trim();
        let start = off + indent;
        let mut blank = true;
        if t.starts_with('#') || t.starts_with("//") {
            let directive = t.trim_start_matches(['#', '/']).trim_start().starts_with('@');
            spans.push((start..off + line.len(), if directive { "attribute" } else { "comment" }));
        } else if let Some(m) = t.split_whitespace().next().filter(|w| METHODS.iter().any(|x| w.eq_ignore_ascii_case(x))) {
            spans.push((start..start + m.len(), "keyword"));
            let path = t[m.len()..].trim_start();
            if !path.is_empty() {
                let at = off + line.len() - line.trim_start().len() + (t.len() - path.len());
                spans.push((at..at + path.trim_end().len(), "string"));
            }
        } else {
            blank = false;
        }
        if blank {
            masked.extend(line.chars().map(|c| if c.is_ascii() { ' ' } else { c }));
            masked.push_str(&raw[line.len()..]);
        } else {
            masked.push_str(raw);
        }
        off += raw.len();
    }
    (masked, spans)
}

/// Byte ranges of the lines that are `-- @…` (`comment` `@…`) comments (indentation excluded).
fn directive_spans(src: &str, comment: &str) -> Vec<Range<usize>> {
    let mut out = vec![];
    let mut off = 0;
    for raw in src.split_inclusive('\n') {
        let line = raw.trim_end_matches(['\n', '\r']);
        let t = line.trim_start();
        if t.strip_prefix(comment).is_some_and(|rest| rest.trim_start().starts_with('@')) {
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
        let spans = directive_spans(src, "--");
        let text: Vec<&str> = spans.iter().map(|r| &src[r.clone()]).collect();
        assert_eq!(text, ["-- @name users", "--@confirm"]);
        let js = "// @connection m\ndb.a.find() // @name no\n";
        assert_eq!(directive_spans(js, "//"), vec![0..16]);
    }

    #[test]
    fn es_lines_are_found_and_masked() {
        let src = "# @connection es\n// note\n  get /a/_search?q=1 \n{\"a\": 1}\r\n";
        let (masked, spans) = es_spans(src);
        assert_eq!(masked.len(), src.len());
        assert!(masked.ends_with("\n{\"a\": 1}\r\n") && masked[..src.find('{').unwrap()].trim().is_empty(), "{masked:?}");
        let got: Vec<(&str, &str)> = spans.iter().map(|(r, role)| (&src[r.clone()], *role)).collect();
        assert_eq!(got, [("# @connection es", "attribute"), ("// note", "comment"), ("get", "keyword"), ("/a/_search?q=1", "string")]);
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

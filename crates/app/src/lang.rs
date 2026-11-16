//! SQL completion on the editor's public LSP-style seam (as Respite's `lang.rs` does for `.http`):
//! computed locally and synchronously by [`crate::complete`] from the file's connection schema.

use std::{cell::RefCell, rc::Rc};

use anyhow::Result;
use gpui::{App, Task, Window};
use gpui_component::input::{CompletionProvider, Rope, RopeExt as _};
use lsp_types::{CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, TextEdit};
use sql_split::Dialect;

use crate::complete::{self, Kind};
use crate::schema_cache::Shared;

/// What the provider needs besides the text; the doc updates it when its connection changes.
pub struct Ctx {
    pub dialect: Dialect,
    /// Schema cache key of the file's connection (`schema_cache::key`).
    pub connection: Option<String>,
    /// Tables of this schema are written without a prefix.
    pub default_schema: Option<String>,
    pub cache: Shared,
}

pub struct Lang(pub Rc<RefCell<Ctx>>);

impl Lang {
    pub fn items(&self, text: &str, offset: usize) -> complete::Completion {
        let ctx = self.0.borrow();
        let schema = ctx.connection.as_ref().and_then(|c| ctx.cache.borrow().schemas.get(c).cloned());
        complete::complete(text, offset, ctx.dialect, schema.as_deref(), ctx.default_schema.as_deref())
    }
}

fn kind(k: Kind) -> CompletionItemKind {
    match k {
        Kind::Keyword | Kind::Directive => CompletionItemKind::KEYWORD,
        Kind::Snippet => CompletionItemKind::SNIPPET,
        Kind::Table => CompletionItemKind::CLASS,
        Kind::View => CompletionItemKind::INTERFACE,
        Kind::Column => CompletionItemKind::FIELD,
        Kind::Function => CompletionItemKind::FUNCTION,
    }
}

impl CompletionProvider for Lang {
    fn completions(&self, text: &Rope, offset: usize, trigger: CompletionContext, _: &mut Window, _: &mut App) -> Task<Result<CompletionResponse>> {
        let src = text.to_string();
        let mut found = self.items(&src, offset);
        // `-` only opens the menu for `-- @name`; elsewhere it is a minus sign.
        if src[..offset.min(src.len())].ends_with('-') && trigger.trigger_kind == lsp_types::CompletionTriggerKind::TRIGGER_CHARACTER {
            found.items.retain(|i| i.kind == Kind::Directive);
        }
        let range = lsp_types::Range::new(text.offset_to_position(found.start), text.offset_to_position(offset));
        let items = found
            .items
            .into_iter()
            .map(|i| CompletionItem {
                label: i.label,
                kind: Some(kind(i.kind)),
                detail: (!i.detail.is_empty()).then_some(i.detail),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text: i.insert })),
                ..Default::default()
            })
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        // Letters refine the word; `.` opens `alias.`; `@` and `-` are `-- @page-size`; quotes open `"Table`.
        !new_text.is_empty() && new_text.chars().all(|c| c.is_alphanumeric() || "_.@-\"`".contains(c))
    }
}

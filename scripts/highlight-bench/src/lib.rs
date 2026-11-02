//! The same work as `crates/atomics/src/highlight.rs`: color every line of a file with bat's grammars.
use syntect::easy::HighlightLines;
use syntect::parsing::SyntaxSet;

pub struct Engine {
    syntaxes: SyntaxSet,
    theme: syntect::highlighting::Theme,
}

impl Engine {
    pub fn new() -> Engine {
        let themes = two_face::theme::extra();
        Engine {
            syntaxes: two_face::syntax::extra_newlines(),
            theme: themes.get(two_face::theme::EmbeddedThemeName::OneHalfDark).clone(),
        }
    }

    /// Returns the number of colored spans, so the work cannot be optimized away.
    pub fn highlight(&self, path: &str, text: &str) -> usize {
        let extension = path.rsplit('.').next().unwrap_or("");
        let syntax = self
            .syntaxes
            .find_syntax_by_extension(extension)
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
        let mut state = HighlightLines::new(syntax, &self.theme);
        let mut spans = 0;
        for line in text.lines() {
            spans += state.highlight_line(&format!("{line}\n"), &self.syntaxes).map_or(0, |regions| regions.len());
        }
        spans
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

/// For the wasm build: what a browser client would call.
#[unsafe(no_mangle)]
pub extern "C" fn highlight_len(path_ptr: *const u8, path_len: usize, text_ptr: *const u8, text_len: usize) -> usize {
    let path = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(path_ptr, path_len)) };
    let text = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(text_ptr, text_len)) };
    Engine::new().highlight(path, text)
}

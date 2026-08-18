//! TOON — Token-Oriented Object Notation.
//!
//! Every byte pilotage returns to the calling agent goes through here. The format
//! hoists repeated object keys into a single header row, which is where the savings
//! over JSON come from:
//!
//! ```text
//! verdict: caution
//! signals[2]{k,v,conf}:
//!   tvl_30d,-42%,high
//!   commits_90d,3,high
//! next: pilotage report r_18f2 --html
//! ```
//!
//! Empty collections render as `key[0]: none` rather than a blank — an agent must
//! never have to guess whether "no output" meant "no data" or "it broke".

use std::fmt::Display;

const INDENT: &str = "  ";

#[derive(Default)]
pub struct Toon {
    buf: String,
    depth: usize,
}

impl Toon {
    pub fn new() -> Self {
        Self::default()
    }

    fn pad(&mut self) {
        for _ in 0..self.depth {
            self.buf.push_str(INDENT);
        }
    }

    fn line(&mut self, s: &str) {
        self.pad();
        self.buf.push_str(s);
        self.buf.push('\n');
    }

    /// `key: value`
    pub fn kv<V: Display>(&mut self, k: &str, v: V) -> &mut Self {
        let v = scalar(&v.to_string());
        self.line(&format!("{k}: {v}"));
        self
    }

    /// `key: value`, skipped entirely when the value is absent. Keeps optional
    /// fields from costing tokens just to say "null".
    pub fn kv_opt<V: Display>(&mut self, k: &str, v: Option<V>) -> &mut Self {
        if let Some(v) = v {
            self.kv(k, v);
        }
        self
    }

    /// `key[tag]: value` — used for identified singletons, e.g. `run[r_18f2]: ...`
    pub fn tagged<V: Display>(&mut self, k: &str, tag: &str, v: V) -> &mut Self {
        let v = scalar(&v.to_string());
        self.line(&format!("{k}[{tag}]: {v}"));
        self
    }

    /// `key[n]: a,b,c` — or `key[0]: none` when empty.
    pub fn list<S: AsRef<str>>(&mut self, k: &str, items: &[S]) -> &mut Self {
        if items.is_empty() {
            return self.empty(k);
        }
        let joined = items
            .iter()
            .map(|s| csv(s.as_ref()))
            .collect::<Vec<_>>()
            .join(",");
        self.line(&format!("{k}[{}]: {joined}", items.len()));
        self
    }

    /// A header row plus one indented line per record — the shape that pays for
    /// itself the moment there is more than one record.
    pub fn table<R: AsRef<[String]>>(&mut self, k: &str, cols: &[&str], rows: &[R]) -> &mut Self {
        if rows.is_empty() {
            return self.empty(k);
        }
        self.line(&format!("{k}[{}]{{{}}}:", rows.len(), cols.join(",")));
        self.depth += 1;
        for row in rows {
            let cells = row
                .as_ref()
                .iter()
                .map(|c| csv(c))
                .collect::<Vec<_>>()
                .join(",");
            self.line(&cells);
        }
        self.depth -= 1;
        self
    }

    /// `key[0]: none` — the definitive empty state.
    pub fn empty(&mut self, k: &str) -> &mut Self {
        self.line(&format!("{k}[0]: none"));
        self
    }

    /// Opens an indented block. Must be paired with [`Toon::end`].
    pub fn section(&mut self, k: &str) -> &mut Self {
        self.line(&format!("{k}:"));
        self.depth += 1;
        self
    }

    pub fn end(&mut self) -> &mut Self {
        self.depth = self.depth.saturating_sub(1);
        self
    }

    /// Contextual disclosure: what the agent should consider running next.
    pub fn next(&mut self, cmds: &[String]) -> &mut Self {
        if !cmds.is_empty() {
            self.line(&format!("next: {}", cmds.join(", ")));
        }
        self
    }

    pub fn render(&self) -> String {
        self.buf.clone()
    }

    pub fn print(&self) {
        print!("{}", self.buf);
    }
}

/// A bare scalar. Only needs quoting when whitespace would be lost or a newline
/// would break the line-oriented format; commas are unambiguous here.
fn scalar(s: &str) -> String {
    let flat = s.replace('\n', "\\n").replace('\r', "");
    if flat.is_empty() {
        return "\"\"".into();
    }
    if flat != s || flat.trim() != flat {
        format!("\"{}\"", flat.replace('"', "\\\""))
    } else {
        flat
    }
}

/// A cell inside a list or table row, where a bare comma would be a field break.
fn csv(s: &str) -> String {
    let flat = s.replace('\n', "\\n").replace('\r', "");
    let needs = flat.is_empty()
        || flat.contains(',')
        || flat.contains('"')
        || flat.trim() != flat
        || flat != s;
    if needs {
        format!("\"{}\"", flat.replace('"', "\\\""))
    } else {
        flat
    }
}

/// Rough token count. Deliberately not a real BPE tokenizer — pilotage would have to
/// carry a vocabulary file to get an exact number, and chars/4 is within a few percent
/// for the ASCII-heavy output this format produces. Used for budgeting and benchmarks,
/// never for billing.
pub fn approx_tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_and_lists() {
        let mut t = Toon::new();
        t.kv("verdict", "caution").list("tags", &["a", "b", "c"]);
        assert_eq!(t.render(), "verdict: caution\ntags[3]: a,b,c\n");
    }

    #[test]
    fn empty_collections_are_explicit() {
        let mut t = Toon::new();
        t.list("results", &[] as &[String]);
        t.table("rows", &["a"], &[] as &[Vec<String>]);
        assert_eq!(t.render(), "results[0]: none\nrows[0]: none\n");
    }

    #[test]
    fn table_hoists_keys_into_one_header() {
        let mut t = Toon::new();
        t.table(
            "signals",
            &["k", "v"],
            &[
                vec!["tvl".to_string(), "-42%".to_string()],
                vec!["commits".to_string(), "3".to_string()],
            ],
        );
        assert_eq!(t.render(), "signals[2]{k,v}:\n  tvl,-42%\n  commits,3\n");
    }

    #[test]
    fn cells_containing_separators_are_quoted() {
        let mut t = Toon::new();
        t.list("x", &["a,b", "c\"d", "plain"]);
        assert_eq!(t.render(), "x[3]: \"a,b\",\"c\\\"d\",plain\n");
    }

    #[test]
    fn newlines_never_break_the_line_format() {
        let mut t = Toon::new();
        t.kv("body", "line1\nline2");
        assert_eq!(t.render(), "body: \"line1\\nline2\"\n");
        assert_eq!(t.render().lines().count(), 1);
    }

    #[test]
    fn sections_indent_and_unindent() {
        let mut t = Toon::new();
        t.kv("a", 1)
            .section("page")
            .kv("title", "x")
            .end()
            .kv("b", 2);
        assert_eq!(t.render(), "a: 1\npage:\n  title: x\nb: 2\n");
    }

    #[test]
    fn table_rows_are_indented_inside_a_section() {
        let mut t = Toon::new();
        t.section("s")
            .table("r", &["c"], &[vec!["v".to_string()]])
            .end();
        assert_eq!(t.render(), "s:\n  r[1]{c}:\n    v\n");
    }

    #[test]
    fn tagged_singletons() {
        let mut t = Toon::new();
        t.tagged("run", "r_18f2", "6 sources, 12.4s");
        assert_eq!(t.render(), "run[r_18f2]: 6 sources, 12.4s\n");
    }

    #[test]
    fn kv_opt_skips_absent_values() {
        let mut t = Toon::new();
        t.kv_opt("a", Some(1)).kv_opt("b", None::<i32>);
        assert_eq!(t.render(), "a: 1\n");
    }

    #[test]
    fn next_is_omitted_when_there_is_nothing_to_suggest() {
        let mut t = Toon::new();
        t.next(&[]);
        assert_eq!(t.render(), "");
    }

    #[test]
    fn beats_json_on_repeated_records() {
        // The whole premise of the format: key hoisting must actually pay.
        let rows: Vec<Vec<String>> = (0..20)
            .map(|i| vec![format!("metric_{i}"), format!("{i}"), "high".into()])
            .collect();
        let mut t = Toon::new();
        t.table("signals", &["key", "value", "confidence"], &rows);
        let toon = t.render();

        let json = serde_json::to_string(
            &rows
                .iter()
                .map(|r| serde_json::json!({"key": r[0], "value": r[1], "confidence": r[2]}))
                .collect::<Vec<_>>(),
        )
        .unwrap();

        assert!(
            toon.len() * 100 / json.len() < 60,
            "expected >40% savings, got toon={} json={}",
            toon.len(),
            json.len()
        );
    }
}

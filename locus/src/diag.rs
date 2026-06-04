//! Structured diagnostics — **one representation, three renderings** (design
//! §8.1: *two renderings, one content*). A checker/CLI response is built once
//! as a [`Report`], then rendered as labelled **text** (the default), a
//! **brief** one-liner, or machine-readable **JSON** (schema `locus-diag/1`).
//!
//! Errors carry a stable **code** (`RN-Exxxx`), a **spec citation** (the
//! calculus section), an optional **location** (a `line:col` — parse errors
//! have one; precise *type*-error spans await a fully spanned AST), and a
//! **hint** where there is an obvious next step.

use crate::{ParseErr, Row, Stage, Type, TypeErr};

/// The versioned diagnostic schema tag.
pub const SCHEMA: &str = "locus-diag/1";

/// A byte range in the source.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    /// The 1-based `(line, column)` of this span's start, given the source.
    pub fn line_col(&self, src: &str) -> (usize, usize) {
        let mut line = 1;
        let mut col = 1;
        for (i, c) in src.char_indices() {
            if i >= self.start {
                break;
            }
            if c == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        (line, col)
    }
}

/// A single checker/CLI response.
#[derive(Clone, Debug)]
pub enum Report {
    /// A successful check — the inferred judgment `type ! row @ stage`.
    Ok { ty: Type, row: Row, stage: Stage },
    /// A parsed AST (pretty-printed), for `ast` mode.
    Ast { pretty: String },
    /// A failure.
    Error {
        phase: &'static str,
        code: &'static str,
        /// The catalog slug paired with `code` (e.g. `match.non-exhaustive`).
        slug: &'static str,
        spec: &'static str,
        message: String,
        /// 1-based `line:col`, when the location is known.
        loc: Option<(usize, usize)>,
        hint: Option<String>,
    },
}

impl Report {
    pub fn parse_error(e: &ParseErr, src: &str) -> Report {
        Report::Error {
            phase: "parse",
            code: "RN-E0001",
            slug: "parse.syntax",
            spec: "the surface grammar",
            message: e.msg.clone(),
            loc: e.span.map(|s| s.line_col(src)),
            hint: None,
        }
    }

    pub fn type_error(e: &TypeErr) -> Report {
        Report::Error {
            phase: "type",
            code: e.code(),
            slug: e.slug(),
            spec: e.spec(),
            message: e.to_string(),
            // precise type-error spans need a spanned AST — a later slice.
            loc: None,
            hint: e.hint(),
        }
    }

    /// Did the check succeed? (Drives the CLI exit code.)
    pub fn ok(&self) -> bool {
        !matches!(self, Report::Error { .. })
    }

    /// **Structured** human text — labelled fields, consistent for ok/error.
    pub fn to_text(&self) -> String {
        match self {
            Report::Ok { ty, row, stage } => {
                format!("ok\n  type  {ty}\n  row   {row}\n  stage {stage}")
            }
            Report::Ast { pretty } => {
                let body = pretty
                    .lines()
                    .map(|l| format!("    {l}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("ok\n  ast\n{body}")
            }
            Report::Error {
                code,
                slug,
                spec,
                message,
                loc,
                hint,
                ..
            } => {
                let mut s = format!("error  {code} {slug}\n  {message}");
                if let Some((l, c)) = loc {
                    s += &format!("\n  at    {l}:{c}");
                }
                s += &format!("\n  spec  {spec}");
                if let Some(h) = hint {
                    s += &format!("\n  hint  {h}");
                }
                s
            }
        }
    }

    /// One-line summary (`--brief`).
    pub fn to_brief(&self) -> String {
        match self {
            Report::Ok { ty, row, stage } => format!("{ty} ! {row} @ {stage}"),
            Report::Ast { pretty } => pretty.split_whitespace().collect::<Vec<_>>().join(" "),
            Report::Error {
                code, message, loc, ..
            } => match loc {
                Some((l, c)) => format!("error[{code}] at {l}:{c}: {message}"),
                None => format!("error[{code}]: {message}"),
            },
        }
    }

    /// Machine-readable JSON (schema `locus-diag/1`).
    pub fn to_json(&self) -> String {
        match self {
            Report::Ok { ty, row, stage } => {
                let labels: Vec<String> = row
                    .labels()
                    .map(|l| format!("\"{}\"", esc(&l.to_string())))
                    .collect();
                format!(
                    "{{\"schema\":\"{SCHEMA}\",\"ok\":true,\"stage\":{stage},\"type\":\"{}\",\"row\":[{}]}}",
                    esc(&ty.to_string()),
                    labels.join(",")
                )
            }
            Report::Ast { pretty } => {
                format!(
                    "{{\"schema\":\"{SCHEMA}\",\"ok\":true,\"ast\":\"{}\"}}",
                    esc(pretty)
                )
            }
            Report::Error {
                phase,
                code,
                slug,
                spec,
                message,
                loc,
                hint,
            } => {
                let loc_field = match loc {
                    Some((l, c)) => format!(",\"line\":{l},\"col\":{c}"),
                    None => String::new(),
                };
                let hint_field = match hint {
                    Some(h) => format!(",\"hint\":\"{}\"", esc(h)),
                    None => String::new(),
                };
                format!(
                    "{{\"schema\":\"{SCHEMA}\",\"ok\":false,\"severity\":\"error\",\"phase\":\"{phase}\",\"code\":\"{code}\",\"slug\":\"{slug}\",\"spec\":\"{}\",\"message\":\"{}\"{loc_field}{hint_field}}}",
                    esc(spec),
                    esc(message)
                )
            }
        }
    }
}

/// Minimal JSON string escaping. (Shared with `sema`'s tree renderer.)
pub(crate) fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Label;

    #[test]
    fn ok_renders_three_ways() {
        let r = Report::Ok {
            ty: Type::Int,
            row: Row::single(Label::World("fs".into())),
            stage: 0,
        };
        assert_eq!(r.to_text(), "ok\n  type  Int\n  row   {fs}\n  stage 0");
        assert_eq!(r.to_brief(), "Int ! {fs} @ 0");
        assert_eq!(
            r.to_json(),
            r#"{"schema":"locus-diag/1","ok":true,"stage":0,"type":"Int","row":["fs"]}"#
        );
        assert!(r.ok());
    }

    #[test]
    fn span_line_col() {
        let src = "ab\ncde";
        assert_eq!(Span { start: 0, end: 1 }.line_col(src), (1, 1));
        assert_eq!(Span { start: 4, end: 5 }.line_col(src), (2, 2)); // 'd'
    }

    #[test]
    fn type_error_has_code_spec_and_hint() {
        let r = Report::type_error(&TypeErr::StageMisuse {
            what: "quote",
            at: 0,
        });
        let text = r.to_text();
        // The `RN-Exxxx slug` style (catalog form): code and slug on one line.
        assert!(text.starts_with("error  RN-E0302 stage.misuse"));
        assert!(text.contains("spec  calculus §3.0"));
        assert!(text.contains("hint  "));
        let json = r.to_json();
        assert!(json.contains("\"code\":\"RN-E0302\""));
        assert!(json.contains("\"slug\":\"stage.misuse\""));
        assert!(json.contains("\"hint\":"));
        assert!(!r.ok());
    }

    #[test]
    fn parse_error_points_at_a_location() {
        // `@` is not a token — the lexer errors at it (column 3).
        let perr = crate::parse("1 @ 2").unwrap_err();
        assert!(perr.span.is_some(), "parse errors carry a span");
        let r = Report::parse_error(&perr, "1 @ 2");
        assert!(r.to_text().contains("at    1:3"));
        assert!(r.to_json().contains("\"line\":1,\"col\":3"));
    }
}

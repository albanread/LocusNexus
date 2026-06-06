//! A small parser: Locus source text → the core [`Term`] AST.
//!
//! **Slice 5** (the grammar) + **slice 8** (spans): the lexer tags each token
//! with its byte [`Span`], so a parse error points at a location. A *minimal,
//! explicit*, ML-style surface — delimiters, not significant indentation —
//! chosen for **simple clarity**: it maps one-to-one onto the core
//! (`syntax.rs`).
//!
//! Grammar (ASCII source):
//! ```text
//!   expr   := "let" id "=" expr "in" expr
//!           | "let" "rec" id ":" type "=" expr "in" expr
//!           | "fn" id (":" type)? "=>" expr
//!           | "extern" string ":" type
//!           | "handle" expr "with" "{" opclause* returnclause? "}"
//!           | "if" expr "then" expr "else" expr
//!           | "case" expr "of" ("|" expr "=>" expr)+ "|" "_" "=>" expr
//!           | "cond" ("|" expr "=>" expr)+ "|" "_" "=>" expr
//!           | "do" "{" dostmt* expr? "}"
//!           | "loop" id "=" expr ("," id "=" expr)* "while" expr "do" expr ("," expr)* ("return" expr | "else" expr | "endloop")
//!           | logic
//!   opclause := id "(" id ")" ("=>" expr | "->" expr) ";"?
//!   returnclause := "return" "(" id ")" "=>" expr
//!   dostmt := "let" id "=" expr ";" | expr ";"
//!   logic := logic_or                               -- short-circuit Bool sugar
//!   logic_or  := logic_and ("||" logic_and)*
//!   logic_and := cmp ("&&" cmp)*
//!   cmp    := bitor (("==" | "!=" | "<" | "<=" | ">" | ">=") bitor)?
//!                                                        -- comparison, non-chaining → Bool
//!   add    := mul (("+" | "+%" | "+?" | "-" | "-%" | "-?") mul)*
//!                                                        -- additive, left-assoc
//!   mul    := app (("*" | "*%" | "*?" | "/" | "%") app)*
//!                                                        -- multiplicative, left-assoc
//!   app    := atom atom*                            -- application, left-assoc
//!   atom   := int | string | "true" | "false" | "~" atom | "(" ")" | "(" expr ")" | id
//!           | "perform" id atom | "quote" "(" expr ")" | "${" expr "}"
//!           | "genlet" "(" expr ")" | "letloc" "{" expr "}"
//!           | "sqrt" atom | "fma" "(" expr "," expr "," expr ")"
//!   type   := atom_type ("->" type ("!" row)?)?
//!   row    := "{" (label ("," label)*)? ("|" rowvar)? "}"
//!   label  := id | "exn" "[" id "]"
//!   atom_type := "Int" | "Bool" | "Unit" | "String" | "Code" "[" type "]"
//!              | "Array" "[" type "]" | ("Pair"|"Quad"|"Oct") "[" type "]"
//!              | ("Pairs"|"Quads"|"Octs") "[" type "]"
//!              | "Mask" "[" ("Pair"|"Quad"|"Oct") "]"
//!              | name ("[" type ("," type)* "]")? | "(" type ")"
//! ```
//! (a `type` declaration's optional parameter list is `"type" name ("[" name ("," name)* "]")? "=" …`)

use crate::diag::Span;
use crate::syntax::{
    BinOp, CastOp, Constraint, Handler, InstanceMethod, Label, Layer, MatchArm, MemWidth,
    ModuleDecl, OpClause, OpDecl, Pattern, ProgramSource, Return, Row, RowVarId, Term,
    TraitMethodSig, Type, VectorShape,
};
use std::collections::{BTreeSet, HashMap};

/// A parse failure: a message and (when known) the [`Span`] it occurred at.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ParseErr {
    pub msg: String,
    pub span: Option<Span>,
}

/// Parse a complete expression from `src`.
pub fn parse(src: &str) -> Result<Term, ParseErr> {
    let toks = tokenize(src)?;
    let mut p = Parser {
        toks,
        pos: 0,
        bar_is_arm: false,
        sugar_id: 0,
        row_vars: HashMap::new(),
        current_mint: None,
    };
    let e = p.expr()?;
    match p.peek() {
        None => Ok(e),
        Some(t) => Err(p.err(format!("trailing tokens, starting at {t:?}"))),
    }
}

/// Parse a **program**: `(module_decl | import_decl)* entry` (`sealing-plan.md`
/// S1a). `module`/`import`/`at`/`seals`/`exposing` are **contextual** keywords —
/// recognised only at the header positions here, so `is_stop_word` is untouched
/// and every existing expression parses byte-for-byte as before. A source with
/// no leading `module`/`import` yields `modules`/`imports` empty and `entry` the
/// whole expression — i.e. exactly what [`parse`] returns, wrapped.
pub fn parse_program(src: &str) -> Result<ProgramSource, ParseErr> {
    let toks = tokenize(src)?;
    let mut p = Parser {
        toks,
        pos: 0,
        bar_is_arm: false,
        sugar_id: 0,
        row_vars: HashMap::new(),
        current_mint: None,
    };
    let mut modules = Vec::new();
    let mut imports = Vec::new();
    loop {
        if p.is_kw("module") {
            modules.push(p.module_decl()?);
        } else if p.is_kw("import") {
            p.eat_kw("import")?;
            imports.push(p.module_name()?);
        } else {
            break;
        }
    }
    let entry = p.expr()?;
    match p.peek() {
        None => Ok(ProgramSource {
            modules,
            imports,
            entry,
        }),
        Some(t) => Err(p.err(format!(
            "trailing tokens after the program entry, starting at {t:?}"
        ))),
    }
}

/// Parse exactly **one module declaration** (a header + body, no trailing entry)
/// and expect end-of-input. Used for the bundled stdlib `.locus` sources, which
/// are module-only files (`sealing-plan.md` S1b).
pub fn parse_module(src: &str) -> Result<ModuleDecl, ParseErr> {
    let toks = tokenize(src)?;
    let mut p = Parser {
        toks,
        pos: 0,
        bar_is_arm: false,
        sugar_id: 0,
        row_vars: HashMap::new(),
        current_mint: None,
    };
    let m = p.module_decl()?;
    match p.peek() {
        None => Ok(m),
        Some(t) => Err(p.err(format!(
            "trailing tokens after the module body, starting at {t:?}"
        ))),
    }
}

/// Parse a single **type** (possibly with effect rows on its arrows) from text,
/// expecting end-of-input. The reader the `.locusi` interface format uses to read
/// a serialized signature back ([`crate::iface`]): the textual type language is
/// exactly the source one — same grammar, same `Type` ([`Type`]'s `Display` is
/// its inverse). Note that an open-row tail (`{… | ρN}`) is not part of the
/// surface grammar; zonked interface signatures are always closed, so this never
/// needs one (a `ρ` ident would not even tokenize).
pub fn type_from_text(src: &str) -> Result<Type, ParseErr> {
    let toks = tokenize(src)?;
    let mut p = Parser {
        toks,
        pos: 0,
        bar_is_arm: false,
        sugar_id: 0,
        row_vars: HashMap::new(),
        current_mint: None,
    };
    let t = p.ty()?;
    match p.peek() {
        None => Ok(t),
        Some(t) => Err(p.err(format!("trailing tokens after the type, starting at {t:?}"))),
    }
}

/// Parse a single effect **label** from its textual (Display) form — the inverse
/// of [`Label`]'s `Display`, exposed for the `.locusi` mints/seals lists
/// ([`crate::iface`]). Recognises `exn[X]`, the boundary powers
/// (`mem`/`winapi`/`crt`/`asm`/`gc`), and user / native op names, via the shared
/// [`effect_label`](Parser::effect_label) reader.
pub fn label_from_text(src: &str) -> Result<Label, ParseErr> {
    let toks = tokenize(src)?;
    let mut p = Parser {
        toks,
        pos: 0,
        bar_is_arm: false,
        sugar_id: 0,
        row_vars: HashMap::new(),
        current_mint: None,
    };
    let l = p.effect_label()?;
    match p.peek() {
        None => Ok(l),
        Some(t) => Err(p.err(format!(
            "trailing tokens after the label, starting at {t:?}"
        ))),
    }
}

// ── lexer ───────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Eq, Debug)]
enum Tok {
    Int(i64),
    Float(u64),
    Str(String),
    Ident(String),
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Colon,
    ColonEq, // :=  (the mutability-v1 assignment `x := v`)
    Semi,
    Comma, // ,  (separates effect-row labels)
    Dot,   // .  (record field access: `r.x`)
    Bang,  // !  (a type's latent effect row: `A -> B ! {mem}`)
    Eq,
    EqEq,         // ==
    Ne,           // !=
    Lt,           // <
    Le,           // <=
    Gt,           // >
    Ge,           // >=
    Plus,         // +
    PlusWrap,     // +%
    PlusChecked,  // +?
    Minus,        // -
    MinusWrap,    // -%
    MinusChecked, // -?
    Star,         // *
    StarWrap,     // *%
    StarChecked,  // *?
    Slash,        // /
    Percent,      // %
    AndAnd,       // &&
    OrOr,         // ||
    Tilde,        // ~
    Amp,          // &
    Bar,          // |
    Caret,        // ^
    Shl,          // <<
    Shr,          // >>
    LArrow,       // <-  (the array store `a[i] <- v`)
    FatArrow,     // =>
    Arrow,        // ->
    DollarBrace,  // ${
}

fn tokenize(src: &str) -> Result<Vec<(Tok, Span)>, ParseErr> {
    let b = src.as_bytes();
    // Skip a leading UTF-8 BOM — Windows editors (and PowerShell's `utf8`
    // encoding) prepend one, and it is not part of the program.
    let mut i = if b.starts_with(&[0xEF, 0xBB, 0xBF]) {
        3
    } else {
        0
    };
    let mut out = Vec::new();
    while i < b.len() {
        let start = i;
        let c = b[i] as char;
        let tok = match c {
            c if c.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            '(' => {
                i += 1;
                Tok::LParen
            }
            ')' => {
                i += 1;
                Tok::RParen
            }
            '{' => {
                i += 1;
                Tok::LBrace
            }
            '}' => {
                i += 1;
                Tok::RBrace
            }
            '[' => {
                i += 1;
                Tok::LBracket
            }
            ']' => {
                i += 1;
                Tok::RBracket
            }
            ':' if b.get(i + 1) == Some(&b'=') => {
                i += 2;
                Tok::ColonEq
            }
            ':' => {
                i += 1;
                Tok::Colon
            }
            ';' => {
                i += 1;
                Tok::Semi
            }
            ',' => {
                i += 1;
                Tok::Comma
            }
            '.' => {
                i += 1;
                Tok::Dot
            }
            '!' if b.get(i + 1) == Some(&b'=') => {
                i += 2;
                Tok::Ne
            }
            '!' => {
                i += 1;
                Tok::Bang
            }
            '=' if b.get(i + 1) == Some(&b'>') => {
                i += 2;
                Tok::FatArrow
            }
            '=' if b.get(i + 1) == Some(&b'=') => {
                i += 2;
                Tok::EqEq
            }
            '=' => {
                i += 1;
                Tok::Eq
            }
            '-' if b.get(i + 1) == Some(&b'>') => {
                i += 2;
                Tok::Arrow
            }
            // `--` to end of line: a line comment (ML / Dylan style).
            '-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            '-' if b.get(i + 1) == Some(&b'%') => {
                i += 2;
                Tok::MinusWrap
            }
            '-' if b.get(i + 1) == Some(&b'?') => {
                i += 2;
                Tok::MinusChecked
            }
            '-' => {
                i += 1;
                Tok::Minus
            }
            '+' if b.get(i + 1) == Some(&b'%') => {
                i += 2;
                Tok::PlusWrap
            }
            '+' if b.get(i + 1) == Some(&b'?') => {
                i += 2;
                Tok::PlusChecked
            }
            '+' => {
                i += 1;
                Tok::Plus
            }
            '*' if b.get(i + 1) == Some(&b'%') => {
                i += 2;
                Tok::StarWrap
            }
            '*' if b.get(i + 1) == Some(&b'?') => {
                i += 2;
                Tok::StarChecked
            }
            '*' => {
                i += 1;
                Tok::Star
            }
            '/' => {
                i += 1;
                Tok::Slash
            }
            '%' => {
                i += 1;
                Tok::Percent
            }
            '<' if b.get(i + 1) == Some(&b'<') => {
                i += 2;
                Tok::Shl
            }
            '<' if b.get(i + 1) == Some(&b'-') => {
                i += 2;
                Tok::LArrow
            }
            '<' if b.get(i + 1) == Some(&b'=') => {
                i += 2;
                Tok::Le
            }
            '<' => {
                i += 1;
                Tok::Lt
            }
            '>' if b.get(i + 1) == Some(&b'=') => {
                i += 2;
                Tok::Ge
            }
            '>' if b.get(i + 1) == Some(&b'>') => {
                i += 2;
                Tok::Shr
            }
            '>' => {
                i += 1;
                Tok::Gt
            }
            '&' if b.get(i + 1) == Some(&b'&') => {
                i += 2;
                Tok::AndAnd
            }
            '&' => {
                i += 1;
                Tok::Amp
            }
            '|' if b.get(i + 1) == Some(&b'|') => {
                i += 2;
                Tok::OrOr
            }
            '|' => {
                i += 1;
                Tok::Bar
            }
            '~' => {
                i += 1;
                Tok::Tilde
            }
            '^' => {
                i += 1;
                Tok::Caret
            }
            '$' if b.get(i + 1) == Some(&b'{') => {
                i += 2;
                Tok::DollarBrace
            }
            // Hex literal `0x…` (or `0X…`) — UTF-8/byte masks read best in hex.
            '0' if matches!(b.get(i + 1), Some(b'x') | Some(b'X')) => {
                i += 2;
                let digits = i;
                while i < b.len() && (b[i] as char).is_ascii_hexdigit() {
                    i += 1;
                }
                if i == digits {
                    return Err(ParseErr {
                        msg: "hex literal `0x` needs at least one digit".into(),
                        span: Some(Span { start, end: i }),
                    });
                }
                let n = i64::from_str_radix(&src[digits..i], 16).map_err(|_| ParseErr {
                    msg: "hex literal out of range".into(),
                    span: Some(Span { start, end: i }),
                })?;
                Tok::Int(n)
            }
            c if c.is_ascii_digit() => {
                while i < b.len() && (b[i] as char).is_ascii_digit() {
                    i += 1;
                }
                let is_float = b.get(i) == Some(&b'.')
                    && b.get(i + 1)
                        .map(|c| (*c as char).is_ascii_digit())
                        .unwrap_or(false);
                if is_float {
                    i += 1; // `.`
                    while i < b.len() && (b[i] as char).is_ascii_digit() {
                        i += 1;
                    }
                    if matches!(b.get(i), Some(b'e') | Some(b'E')) {
                        i += 1;
                        if matches!(b.get(i), Some(b'+') | Some(b'-')) {
                            i += 1;
                        }
                        let exp_digits = i;
                        while i < b.len() && (b[i] as char).is_ascii_digit() {
                            i += 1;
                        }
                        if i == exp_digits {
                            return Err(ParseErr {
                                msg: "float literal exponent needs at least one digit".into(),
                                span: Some(Span { start, end: i }),
                            });
                        }
                    }
                    let f: f64 = src[start..i].parse().map_err(|_| ParseErr {
                        msg: "float literal out of range".into(),
                        span: Some(Span { start, end: i }),
                    })?;
                    if !f.is_finite() {
                        return Err(ParseErr {
                            msg: "float literal out of finite f64 range".into(),
                            span: Some(Span { start, end: i }),
                        });
                    }
                    Tok::Float(f.to_bits())
                } else {
                    let n = src[start..i].parse().map_err(|_| ParseErr {
                        msg: "integer literal out of range".into(),
                        span: Some(Span { start, end: i }),
                    })?;
                    Tok::Int(n)
                }
            }
            '"' => {
                // Scan to the closing quote (skipping escape pairs), then
                // decode escapes. The scan is byte-wise but only matches ASCII
                // `"`/`\`, so it lands cleanly on UTF-8 boundaries.
                let content = i + 1;
                let mut j = content;
                loop {
                    match b.get(j) {
                        None => {
                            return Err(ParseErr {
                                msg: "unterminated string literal".into(),
                                span: Some(Span { start, end: j }),
                            })
                        }
                        Some(&b'"') => break,
                        Some(&b'\\') => j += 2,
                        Some(_) => j += 1,
                    }
                }
                let raw = &src[content..j];
                i = j + 1;
                let mut val = String::new();
                let mut it = raw.chars();
                while let Some(ch) = it.next() {
                    if ch == '\\' {
                        match it.next() {
                            Some('"') => val.push('"'),
                            Some('\\') => val.push('\\'),
                            Some('n') => val.push('\n'),
                            Some('t') => val.push('\t'),
                            other => {
                                return Err(ParseErr {
                                    msg: format!(
                                        "invalid string escape `\\{}`",
                                        other.unwrap_or(' ')
                                    ),
                                    span: Some(Span { start, end: i }),
                                })
                            }
                        }
                    } else {
                        val.push(ch);
                    }
                }
                Tok::Str(val)
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                while i < b.len() && {
                    let d = b[i] as char;
                    d.is_ascii_alphanumeric() || d == '_'
                } {
                    i += 1;
                }
                Tok::Ident(src[start..i].to_string())
            }
            other => {
                return Err(ParseErr {
                    msg: format!("unexpected character {other:?}"),
                    span: Some(Span {
                        start,
                        end: start + 1,
                    }),
                })
            }
        };
        out.push((tok, Span { start, end: i }));
    }
    Ok(out)
}

// ── parser ──────────────────────────────────────────────────────────────

struct Parser {
    toks: Vec<(Tok, Span)>,
    pos: usize,
    /// When set, a top-level `|` is a **match-arm separator**, not bitwise-or, so
    /// `bitor` leaves it for the arm loop. Cleared inside `(`/`[`/`{` (where a
    /// `|` is unambiguously bitwise). Set only while parsing a match-arm body.
    bar_is_arm: bool,
    /// Parser-only counter for hygienic sugar binders. The generated names contain
    /// a control character the lexer cannot produce, so user code cannot capture
    /// or shadow them.
    sugar_id: usize,
    /// Parser-only row variable names seen in annotation rows. The resulting
    /// placeholder ids are rewritten to real unification vars in sema.
    row_vars: HashMap<String, RowVarId>,
    /// The minted label of the `boundary` module whose body is being parsed
    /// (`mints (L)`), stamped onto each `extern` so sema injects `L` rather than
    /// the default `winapi`. `None` outside a minting module (⟹ `winapi`).
    current_mint: Option<Label>,
}

enum DoItem {
    Let(String, Term),
    LetMut(String, Term),
    LetRec(String, Type, Term),
    LetTuple(Vec<String>, Term),
    Expr(String, Term),
}

/// The canonical effect [`Label`] for a name written in a type's effect row
/// (`A -> B ! {mem}`). Extends [`crate::prelude::op_label`] (which knows the
/// native IO ops) with the capability `World` labels `mem` / `winapi` and `gc` —
/// the effects sema creates directly, which are not perform-able runtime ops.
fn row_label(name: &str) -> Label {
    match name {
        "gc" => Label::Gc,
        // The **boundary residual powers** — raw capabilities minted at the FFI /
        // asm edge and lowered to a runtime / foreign call. They are `World`
        // (native residuals, `is_native`), not user effects, so a service may
        // *seal* them. A new boundary provider adds its label to this list (the
        // one place a name is designated a raw power).
        "mem" | "winapi" | "crt" | "libc" | "libm" | "asm" | "agent" => {
            Label::World(name.to_string())
        }
        other => crate::prelude::op_label(other),
    }
}

/// Idents that are *not* the start of an atom.
fn is_stop_word(s: &str) -> bool {
    matches!(
        s,
        "let"
            | "rec"
            | "in"
            | "fn"
            | "handle"
            | "with"
            | "return"
            | "if"
            | "then"
            | "else"
            | "endloop"
            | "loop"
            | "while"
            | "do"
            | "case"
            | "of"
            | "cond"
            | "extern"
            | "type"
            | "match"
    )
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|(t, _)| t)
    }
    /// Span of the current token, or an empty span at end-of-input.
    fn cur_span(&self) -> Option<Span> {
        self.toks.get(self.pos).map(|(_, s)| *s).or_else(|| {
            self.toks.last().map(|(_, s)| Span {
                start: s.end,
                end: s.end,
            })
        })
    }
    fn err(&self, msg: impl Into<String>) -> ParseErr {
        ParseErr {
            msg: msg.into(),
            span: self.cur_span(),
        }
    }
    fn bump(&mut self) {
        self.pos += 1;
    }
    fn eat(&mut self, t: &Tok) -> Result<(), ParseErr> {
        if self.peek() == Some(t) {
            self.bump();
            Ok(())
        } else {
            Err(self.err(format!("expected {t:?}, found {:?}", self.peek())))
        }
    }
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s == kw)
    }
    fn eat_kw(&mut self, kw: &str) -> Result<(), ParseErr> {
        if self.is_kw(kw) {
            self.bump();
            Ok(())
        } else {
            Err(self.err(format!("expected `{kw}`, found {:?}", self.peek())))
        }
    }
    fn ident(&mut self) -> Result<String, ParseErr> {
        match self.peek() {
            Some(Tok::Ident(s)) => {
                let s = s.clone();
                self.bump();
                Ok(s)
            }
            _ => Err(self.err(format!("expected an identifier, found {:?}", self.peek()))),
        }
    }

    fn fresh_sugar_name(&mut self, label: &str) -> String {
        let id = self.sugar_id;
        self.sugar_id += 1;
        format!("\u{1}{label}#{id}")
    }

    fn is_wild_arm(&self) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s == "_")
    }

    fn arm_body(&mut self) -> Result<Term, ParseErr> {
        let saved = self.bar_is_arm;
        self.bar_is_arm = true;
        let body = self.expr();
        self.bar_is_arm = saved;
        body
    }

    fn nest_if_arms(arms: Vec<(Term, Term)>, default: Term) -> Term {
        arms.into_iter().rev().fold(default, |els, (cond, body)| {
            Term::If(Box::new(cond), Box::new(body), Box::new(els))
        })
    }

    fn parse_cond_sugar(&mut self) -> Result<Term, ParseErr> {
        self.eat_kw("cond")?;
        let mut arms = Vec::new();
        let mut default = None;
        let mut saw_arm = false;
        while self.peek() == Some(&Tok::Bar) {
            self.bump();
            saw_arm = true;
            if default.is_some() {
                return Err(self.err("`cond` default `_` arm must be last"));
            }
            if self.is_wild_arm() {
                self.bump();
                self.eat(&Tok::FatArrow)?;
                default = Some(self.arm_body()?);
            } else {
                let pred = self.expr()?;
                self.eat(&Tok::FatArrow)?;
                let body = self.arm_body()?;
                arms.push((pred, body));
            }
        }
        if !saw_arm {
            return Err(self.err("`cond` expects at least one `| predicate => expr` arm"));
        }
        let default = default.ok_or_else(|| self.err("`cond` requires a final `_` default arm"))?;
        Ok(Self::nest_if_arms(arms, default))
    }

    fn parse_case_sugar(&mut self) -> Result<Term, ParseErr> {
        self.eat_kw("case")?;
        let scrutinee = self.expr()?;
        self.eat_kw("of")?;
        let tmp = self.fresh_sugar_name("case");
        let mut arms = Vec::new();
        let mut default = None;
        let mut saw_arm = false;
        while self.peek() == Some(&Tok::Bar) {
            self.bump();
            saw_arm = true;
            if default.is_some() {
                return Err(self.err("`case` default `_` arm must be last"));
            }
            if self.is_wild_arm() {
                self.bump();
                self.eat(&Tok::FatArrow)?;
                default = Some(self.arm_body()?);
            } else {
                let key = self.expr()?;
                self.eat(&Tok::FatArrow)?;
                let body = self.arm_body()?;
                let cond = Term::Bin(BinOp::Eq, Box::new(Term::Var(tmp.clone())), Box::new(key));
                arms.push((cond, body));
            }
        }
        if !saw_arm {
            return Err(self.err("`case` expects at least one `| value => expr` arm"));
        }
        let default = default.ok_or_else(|| self.err("`case` requires a final `_` default arm"))?;
        Ok(Term::Let(
            tmp,
            Box::new(scrutinee),
            Box::new(Self::nest_if_arms(arms, default)),
        ))
    }

    fn nest_do_items(items: Vec<DoItem>, tail: Term) -> Term {
        items.into_iter().rev().fold(tail, |body, item| match item {
            DoItem::Let(name, value) => Term::Let(name, Box::new(value), Box::new(body)),
            DoItem::LetMut(name, value) => Term::LetMut(name, Box::new(value), Box::new(body)),
            DoItem::LetRec(name, ty, value) => {
                Term::LetRec(name, ty, Box::new(value), Box::new(body))
            }
            DoItem::LetTuple(names, value) => {
                Term::LetTuple(names, Box::new(value), Box::new(body))
            }
            DoItem::Expr(name, value) => Term::Let(name, Box::new(value), Box::new(body)),
        })
    }

    fn parse_do_let_item(&mut self) -> Result<DoItem, ParseErr> {
        self.eat_kw("let")?;
        if self.is_kw("mut") {
            self.eat_kw("mut")?;
            let name = self.ident()?;
            self.eat(&Tok::Eq)?;
            let value = self.expr()?;
            Ok(DoItem::LetMut(name, value))
        } else if self.is_kw("rec") {
            self.eat_kw("rec")?;
            let name = self.ident()?;
            self.eat(&Tok::Colon)?;
            let ty = self.ty()?;
            self.eat(&Tok::Eq)?;
            let value = self.expr()?;
            Ok(DoItem::LetRec(name, ty, value))
        } else if self.peek() == Some(&Tok::LParen) {
            self.bump();
            let mut names = vec![self.ident()?];
            while self.peek() == Some(&Tok::Comma) {
                self.bump();
                names.push(self.ident()?);
            }
            self.eat(&Tok::RParen)?;
            self.eat(&Tok::Eq)?;
            let value = self.expr()?;
            Ok(DoItem::LetTuple(names, value))
        } else {
            let name = self.ident()?;
            self.eat(&Tok::Eq)?;
            let value = self.expr()?;
            Ok(DoItem::Let(name, value))
        }
    }

    fn parse_do_sugar(&mut self) -> Result<Term, ParseErr> {
        self.eat_kw("do")?;
        self.eat(&Tok::LBrace)?;
        let mut items = Vec::new();
        let mut tail = None;
        while self.peek() != Some(&Tok::RBrace) {
            if self.is_kw("let") {
                items.push(self.parse_do_let_item()?);
                if self.peek() == Some(&Tok::Semi) {
                    self.bump();
                    continue;
                }
                if self.peek() == Some(&Tok::RBrace) {
                    break;
                }
                return Err(self.err("expected `;` after `do` block let statement"));
            }

            let value = self.expr()?;
            if self.peek() == Some(&Tok::Semi) {
                self.bump();
                let name = self.fresh_sugar_name("do");
                items.push(DoItem::Expr(name, value));
            } else {
                tail = Some(value);
                break;
            }
        }
        self.eat(&Tok::RBrace)?;
        Ok(Self::nest_do_items(items, tail.unwrap_or(Term::Unit)))
    }

    fn effect_label(&mut self) -> Result<Label, ParseErr> {
        let name = self.ident()?;
        if name == "exn" && self.peek() == Some(&Tok::LBracket) {
            self.bump();
            let exn = self.ident()?;
            self.eat(&Tok::RBracket)?;
            Ok(Label::Exn(exn))
        } else {
            Ok(row_label(&name))
        }
    }

    fn row_var(&mut self, name: String) -> RowVarId {
        if let Some(&id) = self.row_vars.get(&name) {
            return id;
        }
        let id = RowVarId::parsed(self.row_vars.len() as u32);
        self.row_vars.insert(name, id);
        id
    }

    /// Parse a `type` declaration **head** — `type Name ("[" p, … "]")? "="
    /// variant ("|" variant)*` — up to (but not including) the `in`. Shared by
    /// [`expr`](Self::expr) (`type … in body`) and [`module_body`](Self::module_body)
    /// (a module's let/type chain). Each variant is `(ctor, field_types)`.
    fn type_decl_head(
        &mut self,
    ) -> Result<(String, Vec<String>, Vec<(String, Vec<Type>)>), ParseErr> {
        self.eat_kw("type")?;
        let name = self.ident()?;
        let params = if self.peek() == Some(&Tok::LBracket) {
            self.bump();
            let mut ps = vec![self.ident()?];
            while self.peek() == Some(&Tok::Comma) {
                self.bump();
                ps.push(self.ident()?);
            }
            self.eat(&Tok::RBracket)?;
            ps
        } else {
            Vec::new()
        };
        self.eat(&Tok::Eq)?;
        let mut variants = Vec::new();
        loop {
            let ctor = self.ident()?;
            let fields = if self.peek() == Some(&Tok::LParen) {
                self.bump();
                let mut tys = vec![self.ty()?];
                while self.peek() == Some(&Tok::Comma) {
                    self.bump();
                    tys.push(self.ty()?);
                }
                self.eat(&Tok::RParen)?;
                tys
            } else {
                Vec::new()
            };
            variants.push((ctor, fields));
            if self.peek() == Some(&Tok::Bar) {
                self.bump();
            } else {
                break;
            }
        }
        Ok((name, params, variants))
    }

    /// Parse a **trait declaration head** — `trait Name a ("requires" C a, …)?
    /// "{" (m ":" sig (";" m ":" sig)*)? "}"` — up to (but not including) the
    /// `in`/body. Shared by [`expr`](Self::expr) and
    /// [`module_body`](Self::module_body), like [`type_decl_head`](Self::type_decl_head).
    /// `trait`/`requires` are **contextual** keywords (recognised only here, like
    /// `module`/`at`/`seals`); `is_stop_word` is untouched.
    fn trait_decl_head(
        &mut self,
    ) -> Result<(String, String, Vec<Constraint>, Vec<TraitMethodSig>), ParseErr> {
        self.eat_kw("trait")?;
        let name = self.ident()?;
        let param = self.ident()?;
        let supers = if self.is_kw("requires") {
            self.eat_kw("requires")?;
            self.constraint_list()?
        } else {
            Vec::new()
        };
        self.eat(&Tok::LBrace)?;
        let mut methods = Vec::new();
        while self.peek() != Some(&Tok::RBrace) {
            let mname = self.ident()?;
            self.eat(&Tok::Colon)?;
            // A method signature may itself be qualified / row-carrying; the
            // trait's own `Trait a` constraint is added by sema (it is implicit).
            // We keep only the underlying type here (the qualified prefix on a
            // method sig is recorded as a superclass-style obligation in a later
            // sprint; for v1 the surface allows it but it is folded into the type).
            let (_qual, sig) = self.qualified_ty()?;
            methods.push(TraitMethodSig { name: mname, sig });
            if self.peek() == Some(&Tok::Semi) {
                self.bump();
            } else {
                break;
            }
        }
        self.eat(&Tok::RBrace)?;
        Ok((name, param, supers, methods))
    }

    /// Parse an **instance declaration head** — `instance Name Type
    /// ("requires" C a, …)? "{" (m "=" e (";" m "=" e)*)? "}"` — up to the
    /// `in`/body. Shared by [`expr`](Self::expr) and
    /// [`module_body`](Self::module_body). The head type is a single type atom (so
    /// `instance Show Int` / `instance Show List[a]` work — a trait head is one
    /// constructor, never a bare arrow); `instance`/`requires` are contextual.
    fn instance_decl_head(
        &mut self,
    ) -> Result<(String, Type, Vec<Constraint>, Vec<InstanceMethod>), ParseErr> {
        self.eat_kw("instance")?;
        let trait_name = self.ident()?;
        let head = self.atom_ty()?;
        let requires = if self.is_kw("requires") {
            self.eat_kw("requires")?;
            self.constraint_list()?
        } else {
            Vec::new()
        };
        self.eat(&Tok::LBrace)?;
        let mut methods = Vec::new();
        while self.peek() != Some(&Tok::RBrace) {
            let mname = self.ident()?;
            self.eat(&Tok::Eq)?;
            let body = self.expr()?;
            methods.push(InstanceMethod { name: mname, body });
            if self.peek() == Some(&Tok::Semi) {
                self.bump();
            } else {
                break;
            }
        }
        self.eat(&Tok::RBrace)?;
        Ok((trait_name, head, requires, methods))
    }

    /// A (possibly dotted) module name: `Kernel.Console`. The `.` joins segments
    /// into one string (no field-access meaning at a module-header position).
    fn module_name(&mut self) -> Result<String, ParseErr> {
        let mut name = self.ident()?;
        while self.peek() == Some(&Tok::Dot) {
            self.bump();
            name.push('.');
            name.push_str(&self.ident()?);
        }
        Ok(name)
    }

    /// Parse one module declaration: `module Name at <layer> ("mints" "("
    /// label,* ")")? ("seals" "(" label,* ")")? ("exposing" "(" id,* ")")? "="
    /// body`. The `mints (L)` label is stamped onto every `extern` in the body so
    /// sema injects `L` rather than the default `winapi`.
    fn module_decl(&mut self) -> Result<ModuleDecl, ParseErr> {
        self.eat_kw("module")?;
        let name = self.module_name()?;
        self.eat_kw("at")?;
        let layer_name = self.ident()?;
        let layer = Layer::from_name(&layer_name).ok_or_else(|| {
            self.err(format!(
                "unknown layer `{layer_name}` — one of `boundary`, `services`, `app`"
            ))
        })?;
        let mints = if self.is_kw("mints") {
            self.eat_kw("mints")?;
            self.paren_list(Self::effect_label)?
        } else {
            Vec::new()
        };
        let seals = if self.is_kw("seals") {
            self.eat_kw("seals")?;
            self.paren_list(Self::effect_label)?
        } else {
            Vec::new()
        };
        let exposing = if self.is_kw("exposing") {
            self.eat_kw("exposing")?;
            Some(self.paren_list(Self::ident)?)
        } else {
            None
        };
        self.eat(&Tok::Eq)?;
        // Stamp the (first) minted label onto the body's `extern`s while parsing.
        let saved_mint = self.current_mint.take();
        self.current_mint = mints.first().cloned();
        let body = self.module_body()?;
        self.current_mint = saved_mint;
        Ok(ModuleDecl {
            name,
            layer,
            mints,
            seals,
            exposing,
            body,
        })
    }

    /// `"(" item ("," item)* ")"` (or `"(" ")"`), each item parsed by `f`. Used
    /// for the `seals (…)` / `exposing (…)` header clauses.
    fn paren_list<T>(
        &mut self,
        mut f: impl FnMut(&mut Self) -> Result<T, ParseErr>,
    ) -> Result<Vec<T>, ParseErr> {
        self.eat(&Tok::LParen)?;
        let mut items = Vec::new();
        if self.peek() != Some(&Tok::RParen) {
            items.push(f(self)?);
            while self.peek() == Some(&Tok::Comma) {
                self.bump();
                items.push(f(self)?);
            }
        }
        self.eat(&Tok::RParen)?;
        Ok(items)
    }

    /// Parse a **module body** — the existing let/type chain ending in the `()`
    /// placeholder, or a `handle … with { … }` wrap (the seal pattern). A
    /// **restricted** grammar that matches `()` as a *literal terminator*: the
    /// chain self-terminates at `()` (or `}`), so a greedy `app` never absorbs
    /// the following declaration or the program entry (`sealing-plan.md` S1a, the
    /// load-bearing body grammar). Bound RHS expressions use the full `expr`,
    /// which already stops at `in` (a reserved stop-word).
    fn module_body(&mut self) -> Result<Term, ParseErr> {
        if self.is_kw("handle") {
            // The handler-wrap case: `parse_handle` ends at its own `}`.
            return self.parse_handle();
        }
        if self.is_kw("let") {
            self.eat_kw("let")?;
            if self.is_kw("rec") {
                self.eat_kw("rec")?;
                let f = self.ident()?;
                self.eat(&Tok::Colon)?;
                let ty = self.ty()?;
                self.eat(&Tok::Eq)?;
                let e1 = self.expr()?;
                self.eat_kw("in")?;
                let body = self.module_body()?;
                return Ok(Term::LetRec(f, ty, Box::new(e1), Box::new(body)));
            }
            if self.peek() == Some(&Tok::LParen) {
                self.bump();
                let mut names = vec![self.ident()?];
                while self.peek() == Some(&Tok::Comma) {
                    self.bump();
                    names.push(self.ident()?);
                }
                self.eat(&Tok::RParen)?;
                self.eat(&Tok::Eq)?;
                let e1 = self.expr()?;
                self.eat_kw("in")?;
                let body = self.module_body()?;
                return Ok(Term::LetTuple(names, Box::new(e1), Box::new(body)));
            }
            let x = self.ident()?;
            self.eat(&Tok::Eq)?;
            let e1 = self.expr()?;
            self.eat_kw("in")?;
            let body = self.module_body()?;
            return Ok(Term::Let(x, Box::new(e1), Box::new(body)));
        }
        if self.is_kw("type") {
            let (name, params, variants) = self.type_decl_head()?;
            self.eat_kw("in")?;
            let body = self.module_body()?;
            return Ok(Term::TypeDef {
                name,
                params,
                variants,
                module: None,
                body: Box::new(body),
            });
        }
        if self.is_kw("effect") {
            self.eat_kw("effect")?;
            let name = self.ident()?;
            let ops = if self.peek() == Some(&Tok::LBrace) {
                self.bump();
                let mut ops = Vec::new();
                while self.peek() != Some(&Tok::RBrace) {
                    ops.push(self.op_decl()?);
                    if self.peek() == Some(&Tok::Semi) {
                        self.bump();
                    } else {
                        break;
                    }
                }
                self.eat(&Tok::RBrace)?;
                ops
            } else {
                self.eat(&Tok::Colon)?;
                let (param, result) = self.op_sig()?;
                vec![OpDecl {
                    op: name.clone(),
                    param,
                    result,
                }]
            };
            self.eat_kw("in")?;
            let body = self.module_body()?;
            return Ok(Term::Effect {
                name,
                ops,
                body: Box::new(body),
            });
        }
        if self.is_kw("trait") {
            let (name, param, supers, methods) = self.trait_decl_head()?;
            self.eat_kw("in")?;
            let body = self.module_body()?;
            return Ok(Term::Trait {
                name,
                param,
                supers,
                methods,
                module: None,
                body: Box::new(body),
            });
        }
        if self.is_kw("instance") {
            let (trait_name, head, requires, methods) = self.instance_decl_head()?;
            self.eat_kw("in")?;
            let body = self.module_body()?;
            return Ok(Term::Instance {
                trait_name,
                head,
                requires,
                methods,
                module: None,
                body: Box::new(body),
            });
        }
        // The terminator: the `()` placeholder. STOP after `)`.
        self.eat(&Tok::LParen)?;
        self.eat(&Tok::RParen)?;
        Ok(Term::Unit)
    }

    fn expr(&mut self) -> Result<Term, ParseErr> {
        // `x := e` — assign a mutable local (mutability v1). `:=` is a low-
        // precedence assignment, parsed at the loosest expression level (like the
        // `let`/`if` forms below), parallel to the array store `a[i] <- v` in
        // `postfix`. We peek `Ident` followed by `:=`; a bare `x` without `:=`
        // falls through to `cmp` exactly as before.
        if let (Some(Tok::Ident(x)), Some((Tok::ColonEq, _))) =
            (self.peek(), self.toks.get(self.pos + 1))
        {
            if !is_stop_word(x) {
                let x = x.clone();
                self.bump(); // the identifier
                self.bump(); // `:=`
                let value = self.expr()?;
                return Ok(Term::Assign(x, Box::new(value)));
            }
        }
        if self.is_kw("let") {
            self.eat_kw("let")?;
            if self.is_kw("mut") {
                // `let mut x = e1 in e2` — a non-escaping scalar mutable local
                // (mutability v1). `mut` is a **contextual** keyword: recognised
                // only here, right after `let`, so `is_stop_word` is untouched and
                // every existing program parses byte-for-byte as before.
                self.eat_kw("mut")?;
                let x = self.ident()?;
                self.eat(&Tok::Eq)?;
                let e1 = self.expr()?;
                self.eat_kw("in")?;
                let e2 = self.expr()?;
                Ok(Term::LetMut(x, Box::new(e1), Box::new(e2)))
            } else if self.is_kw("rec") {
                self.eat_kw("rec")?;
                let f = self.ident()?;
                self.eat(&Tok::Colon)?;
                let ty = self.ty()?;
                self.eat(&Tok::Eq)?;
                let e1 = self.expr()?;
                self.eat_kw("in")?;
                let e2 = self.expr()?;
                Ok(Term::LetRec(f, ty, Box::new(e1), Box::new(e2)))
            } else if self.peek() == Some(&Tok::LParen) {
                // `let (x1, …, xn) = e in body` — tuple destructuring.
                self.bump();
                let mut names = vec![self.ident()?];
                while self.peek() == Some(&Tok::Comma) {
                    self.bump();
                    names.push(self.ident()?);
                }
                self.eat(&Tok::RParen)?;
                self.eat(&Tok::Eq)?;
                let e1 = self.expr()?;
                self.eat_kw("in")?;
                let e2 = self.expr()?;
                Ok(Term::LetTuple(names, Box::new(e1), Box::new(e2)))
            } else {
                let x = self.ident()?;
                self.eat(&Tok::Eq)?;
                let e1 = self.expr()?;
                self.eat_kw("in")?;
                let e2 = self.expr()?;
                Ok(Term::Let(x, Box::new(e1), Box::new(e2)))
            }
        } else if self.is_kw("fn") {
            self.eat_kw("fn")?;
            let x = self.ident()?;
            // The `: T` parameter annotation is **optional** (S2). Omit it
            // (`fn x => …`) and sema infers the parameter type with a fresh
            // unification variable from the body / the lambda's uses.
            let t = if matches!(self.peek(), Some(Tok::Colon)) {
                self.eat(&Tok::Colon)?;
                Some(self.ty()?)
            } else {
                None
            };
            self.eat(&Tok::FatArrow)?;
            let body = self.expr()?;
            Ok(Term::Lam(x, t, Box::new(body)))
        } else if self.is_kw("handle") {
            self.parse_handle()
        } else if self.is_kw("if") {
            self.eat_kw("if")?;
            let cond = self.expr()?;
            self.eat_kw("then")?;
            let then = self.expr()?;
            self.eat_kw("else")?;
            let els = self.expr()?;
            Ok(Term::If(Box::new(cond), Box::new(then), Box::new(els)))
        } else if self.is_kw("case") {
            self.parse_case_sugar()
        } else if self.is_kw("cond") {
            self.parse_cond_sugar()
        } else if self.is_kw("do") {
            self.parse_do_sugar()
        } else if self.is_kw("loop") {
            self.eat_kw("loop")?;
            let mut vars = Vec::new();
            loop {
                let name = self.ident()?;
                self.eat(&Tok::Eq)?;
                let init = self.expr()?;
                vars.push((name, init));
                if self.peek() == Some(&Tok::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
            self.eat_kw("while")?;
            let cond = self.expr()?;
            self.eat_kw("do")?;
            let mut steps = Vec::new();
            loop {
                steps.push(self.expr()?);
                if self.peek() == Some(&Tok::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
            let result = if self.is_kw("return") {
                self.eat_kw("return")?;
                self.expr()?
            } else if self.is_kw("else") {
                self.eat_kw("else")?;
                self.expr()?
            } else if self.is_kw("endloop") {
                self.eat_kw("endloop")?;
                Term::Unit
            } else {
                return Err(self.err("expected `return`, `else`, or `endloop` after loop steps"));
            };
            // Multi-variable loop with a SINGLE step expression: treat the step
            // as a *tuple* of the next accumulator values — evaluated once per
            // iteration and destructured into the accumulators. This makes the
            // natural shapes work without the packed-Int workaround:
            //
            //   loop a, b, c while p do let (x,y,z) = m in (x, y, z) else f
            //   loop a, b, c while p do (match e with | P => (..) | ..) else f
            //
            // because a comma-separated step list can't share a `let`/`match`
            // across its elements (the `let … in` body or a match arm is a
            // single expr; the commas belong to the loop). Desugar to a
            // single-accumulator loop over a tuple, rebinding the names from it
            // in the condition / step / result — reusing tuple destructuring,
            // so no change to elaboration or lowering. This only fires for
            // `vars > 1 && steps == 1`, which was previously always a
            // `LoopArity` error, so no existing program changes meaning.
            if vars.len() > 1 && steps.len() == 1 {
                let names: Vec<String> = vars.iter().map(|(n, _)| n.clone()).collect();
                let inits: Vec<Term> = vars.into_iter().map(|(_, i)| i).collect();
                let step = steps.into_iter().next().expect("steps.len() == 1");
                // A binder the user is exceedingly unlikely to collide with; even
                // if they did, the destructuring binds values eagerly, so a
                // shadow is harmless.
                let tup = "__loop_tuple";
                let wrap = |body: Term| {
                    Term::LetTuple(
                        names.clone(),
                        Box::new(Term::Var(tup.to_string())),
                        Box::new(body),
                    )
                };
                return Ok(Term::Loop {
                    vars: vec![(tup.to_string(), Term::Tuple(inits))],
                    cond: Box::new(wrap(cond)),
                    steps: vec![wrap(step)],
                    result: Box::new(wrap(result)),
                });
            }
            Ok(Term::Loop {
                vars,
                cond: Box::new(cond),
                steps,
                result: Box::new(result),
            })
        } else if self.is_kw("type") {
            // `type Name[a, …] = C1(T..) | C2 | … in body`. The head is shared
            // with `module_body` (a module's let/type chain), so it lives in
            // `type_decl_head`; here we attach the `in body` continuation.
            let (name, params, variants) = self.type_decl_head()?;
            self.eat_kw("in")?;
            let body = self.expr()?;
            Ok(Term::TypeDef {
                name,
                params,
                variants,
                module: None,
                body: Box::new(body),
            })
        } else if self.is_kw("trait") {
            // `trait Name a [requires …] { m : sig ; … } in body` (traits v1). The
            // head is shared with `module_body`; here we attach the `in body`.
            let (name, param, supers, methods) = self.trait_decl_head()?;
            self.eat_kw("in")?;
            let body = self.expr()?;
            Ok(Term::Trait {
                name,
                param,
                supers,
                methods,
                module: None,
                body: Box::new(body),
            })
        } else if self.is_kw("instance") {
            // `instance Name Type [requires …] { m = e ; … } in body` (traits v1).
            let (trait_name, head, requires, methods) = self.instance_decl_head()?;
            self.eat_kw("in")?;
            let body = self.expr()?;
            Ok(Term::Instance {
                trait_name,
                head,
                requires,
                methods,
                module: None,
                body: Box::new(body),
            })
        } else if self.is_kw("match") {
            // `match e with | pat => body | …`
            self.eat_kw("match")?;
            let scrutinee = self.expr()?;
            self.eat_kw("with")?;
            let mut arms = Vec::new();
            while self.peek() == Some(&Tok::Bar) {
                self.bump();
                let pat = self.pattern()?;
                self.eat(&Tok::FatArrow)?;
                // Parse the arm body with `|` as the arm separator (so the body
                // doesn't swallow the next arm); a bitwise `|` in the body must be
                // parenthesised.
                let body = self.arm_body()?;
                arms.push(MatchArm { pat, body });
            }
            Ok(Term::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            })
        } else if self.is_kw("extern") {
            self.eat_kw("extern")?;
            // `extern asm "sym" : T` — a separately-assembled Layer-0 symbol (D5).
            // The `asm` provider; the type is **required** (no oracle supplies it).
            let is_asm = self.is_kw("asm");
            if is_asm {
                self.eat_kw("asm")?;
            }
            let sym = match self.peek() {
                Some(Tok::Str(s)) => {
                    let s = s.clone();
                    self.bump();
                    s
                }
                _ => return Err(self.err("`extern` expects a \"symbol\" string")),
            };
            if is_asm {
                self.eat(&Tok::Colon)
                    .map_err(|_| self.err("`extern asm` requires an explicit `: T` signature"))?;
                let ty = self.ty()?;
                return Ok(Term::ExternAsm(sym, ty));
            }
            // The `: T` signature is optional — omit it and the Win32 oracle
            // supplies it (resolved before elaboration; the std-only checker
            // requires an explicit type).
            let ty = if matches!(self.peek(), Some(Tok::Colon)) {
                self.eat(&Tok::Colon)?;
                Some(self.ty()?)
            } else {
                None
            };
            Ok(Term::Extern(sym, ty, self.current_mint.clone()))
        } else if self.is_kw("effect") {
            // `effect name : Param -> Result in body`            (one op), or
            // `effect Name { op : P -> R ; … } in body`          (several ops).
            self.eat_kw("effect")?;
            let name = self.ident()?;
            let ops = if self.peek() == Some(&Tok::LBrace) {
                self.bump();
                let mut ops = Vec::new();
                while self.peek() != Some(&Tok::RBrace) {
                    ops.push(self.op_decl()?);
                    if self.peek() == Some(&Tok::Semi) {
                        self.bump();
                    } else {
                        break;
                    }
                }
                self.eat(&Tok::RBrace)?;
                ops
            } else {
                // single op: the effect name *is* the operation name.
                self.eat(&Tok::Colon)?;
                let (param, result) = self.op_sig()?;
                vec![OpDecl {
                    op: name.clone(),
                    param,
                    result,
                }]
            };
            self.eat_kw("in")?;
            let body = self.expr()?;
            Ok(Term::Effect {
                name,
                ops,
                body: Box::new(body),
            })
        } else {
            self.logic_or()
        }
    }

    // Precedence ladder, loosest → tightest:
    //   cmp (== != < <= > >=) → bitor (| ^) → bitand (&) → add (+ -) → shift (<< >>)
    //   → mul (*) → app
    // Comparison is the LOOSEST (so `a & b == c` is `(a & b) == c`, not C's
    // footgun `a & (b == c)`); bitwise binds looser than arithmetic; shifts sit
    // just under multiply (so `x << 2 + 1` is `(x << 2) + 1`).

    /// Logical OR `logic_and ("||" logic_and)*`, short-circuiting.
    fn logic_or(&mut self) -> Result<Term, ParseErr> {
        let mut lhs = self.logic_and()?;
        while self.peek() == Some(&Tok::OrOr) {
            self.bump();
            let rhs = self.logic_and()?;
            lhs = Term::If(Box::new(lhs), Box::new(Term::Bool(true)), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// Logical AND `cmp ("&&" cmp)*`, short-circuiting.
    fn logic_and(&mut self) -> Result<Term, ParseErr> {
        let mut lhs = self.cmp()?;
        while self.peek() == Some(&Tok::AndAnd) {
            self.bump();
            let rhs = self.cmp()?;
            lhs = Term::If(Box::new(lhs), Box::new(rhs), Box::new(Term::Bool(false)));
        }
        Ok(lhs)
    }

    /// Comparison `bitor (("==" | "!=" | "<" | "<=" | ">" | ">=") bitor)?`
    /// — non-chaining, yields `Bool`.
    fn cmp(&mut self) -> Result<Term, ParseErr> {
        let lhs = self.bitor()?;
        let op = match self.peek() {
            Some(Tok::EqEq) => BinOp::Eq,
            Some(Tok::Ne) => BinOp::Ne,
            Some(Tok::Lt) => BinOp::Lt,
            Some(Tok::Le) => BinOp::Le,
            Some(Tok::Gt) => BinOp::Gt,
            Some(Tok::Ge) => BinOp::Ge,
            _ => return Ok(lhs),
        };
        self.bump();
        let rhs = self.bitor()?;
        Ok(Term::Bin(op, Box::new(lhs), Box::new(rhs)))
    }

    /// Bitwise OR / XOR `bitand (("|" | "^") bitand)*` — left-associative.
    fn bitor(&mut self) -> Result<Term, ParseErr> {
        let mut lhs = self.bitand()?;
        loop {
            let op = match self.peek() {
                // A top-level `|` in a match-arm body ends the arm, not bitwise-or.
                Some(Tok::Bar) if self.bar_is_arm => break,
                Some(Tok::Bar) => BinOp::Or,
                Some(Tok::Caret) => BinOp::Xor,
                _ => break,
            };
            self.bump();
            let rhs = self.bitand()?;
            lhs = Term::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// Bitwise AND `add ("&" add)*` — left-associative, binds tighter than `|`.
    fn bitand(&mut self) -> Result<Term, ParseErr> {
        let mut lhs = self.add()?;
        while self.peek() == Some(&Tok::Amp) {
            self.bump();
            let rhs = self.add()?;
            lhs = Term::Bin(BinOp::And, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// Additive `shift (("+" | "+%" | "+?" | "-" | "-%" | "-?") shift)*`.
    fn add(&mut self) -> Result<Term, ParseErr> {
        let mut lhs = self.shift()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => BinOp::Add,
                Some(Tok::PlusWrap) => BinOp::AddWrap,
                Some(Tok::PlusChecked) => BinOp::AddChecked,
                Some(Tok::Minus) => BinOp::Sub,
                Some(Tok::MinusWrap) => BinOp::SubWrap,
                Some(Tok::MinusChecked) => BinOp::SubChecked,
                _ => break,
            };
            self.bump();
            let rhs = self.shift()?;
            lhs = Term::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// Shift `mul (("<<" | ">>") mul)*` — left-associative, just under multiply.
    fn shift(&mut self) -> Result<Term, ParseErr> {
        let mut lhs = self.mul()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Shl) => BinOp::Shl,
                Some(Tok::Shr) => BinOp::Shr,
                _ => break,
            };
            self.bump();
            let rhs = self.mul()?;
            lhs = Term::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// Multiplicative `app (("*" | "*%" | "*?" | "/" | "%") app)*`.
    fn mul(&mut self) -> Result<Term, ParseErr> {
        let mut lhs = self.app()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => BinOp::Mul,
                Some(Tok::StarWrap) => BinOp::MulWrap,
                Some(Tok::StarChecked) => BinOp::MulChecked,
                Some(Tok::Slash) => BinOp::Div,
                Some(Tok::Percent) => BinOp::Mod,
                _ => break,
            };
            self.bump();
            let rhs = self.app()?;
            lhs = Term::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_handle(&mut self) -> Result<Term, ParseErr> {
        self.eat_kw("handle")?;
        let scrutinee = self.expr()?;
        self.eat_kw("with")?;
        self.eat(&Tok::LBrace)?;
        let mut ops = Vec::new();
        while !self.is_kw("return") && self.peek() != Some(&Tok::RBrace) {
            let op = self.ident()?;
            self.eat(&Tok::LParen)?;
            let arg = self.ident()?;
            self.eat(&Tok::RParen)?;
            let body = match self.peek() {
                Some(Tok::FatArrow) => {
                    self.bump();
                    self.expr()?
                }
                Some(Tok::Arrow) => {
                    self.bump();
                    let value = self.expr()?;
                    Term::App(Box::new(Term::Var("resume".into())), Box::new(value))
                }
                _ => return Err(self.err("expected `=>` or `->` in handler arm")),
            };
            if self.peek() == Some(&Tok::Semi) {
                self.bump();
            } else if !self.is_kw("return") && self.peek() != Some(&Tok::RBrace) {
                return Err(self.err("expected `;` between handler arms"));
            }
            ops.push(OpClause {
                // Same canonicalisation as `perform`, so a `console(s) => …`
                // clause discharges a `perform console …`.
                op: crate::prelude::op_label(&op),
                arg,
                resume: "resume".to_string(),
                body: Box::new(body),
            });
        }
        let ret = if self.is_kw("return") {
            self.eat_kw("return")?;
            self.eat(&Tok::LParen)?;
            let var = self.ident()?;
            self.eat(&Tok::RParen)?;
            self.eat(&Tok::FatArrow)?;
            let rbody = self.expr()?;
            Return {
                var,
                body: Box::new(rbody),
            }
        } else {
            let var = self.fresh_sugar_name("return");
            Return {
                var: var.clone(),
                body: Box::new(Term::Var(var)),
            }
        };
        self.eat(&Tok::RBrace)?;
        Ok(Term::Handle(
            Box::new(scrutinee),
            Box::new(Handler { ops, ret }),
        ))
    }

    fn app(&mut self) -> Result<Term, ParseErr> {
        let mut e = self.atom()?;
        while self.starts_atom() {
            let arg = self.atom()?;
            e = Term::App(Box::new(e), Box::new(arg));
        }
        Ok(e)
    }

    fn starts_atom(&self) -> bool {
        match self.peek() {
            Some(Tok::Int(_))
            | Some(Tok::Float(_))
            | Some(Tok::Str(_))
            | Some(Tok::LParen)
            // `~b` is Bool negation sugar, kept away from the effect-row `!`
            // spelling. It heads an atom so `~b && c` is `(~b) && c`.
            | Some(Tok::Tilde)
            // A leading `!` heads a deref atom (`f !r` = `f (!r)`); unambiguous in
            // expression position (the type-row `!` only follows `->` in a type).
            | Some(Tok::Bang)
            | Some(Tok::DollarBrace) => true,
            Some(Tok::Ident(s)) => !is_stop_word(s),
            _ => false,
        }
    }

    fn atom(&mut self) -> Result<Term, ParseErr> {
        let base = self.atom_base()?;
        self.postfix(base)
    }

    /// Postfix subscripting — the array accessor `a[i]` (read) and its store
    /// `a[i] <- v`. Binds as tightly as an atom and chains (`a[i][j]`), so an
    /// index argument is parenthesised in application: `f a[i]` = `f (a[i])`.
    fn postfix(&mut self, mut e: Term) -> Result<Term, ParseErr> {
        loop {
            match self.peek() {
                Some(Tok::LBracket) => {
                    self.bump();
                    let idx = self.expr()?;
                    self.eat(&Tok::RBracket)?;
                    e = if self.peek() == Some(&Tok::LArrow) {
                        self.bump();
                        // The stored value is a full expression up to (not including)
                        // a comparison — arithmetic and bitwise (`out[j] <- 0x80 | b`).
                        let val = self.cmp()?;
                        Term::IndexSet(Box::new(e), Box::new(idx), Box::new(val))
                    } else {
                        Term::Index(Box::new(e), Box::new(idx))
                    };
                }
                // `r.x` — record field access (chains: `r.x.y`).
                Some(Tok::Dot) => {
                    self.bump();
                    let field = self.ident()?;
                    e = Term::Field(Box::new(e), field);
                }
                _ => break,
            }
        }
        Ok(e)
    }

    /// One `match` pattern: `_`, a nullary `C`, or `C(x, y, …)` binding fields.
    fn pattern(&mut self) -> Result<Pattern, ParseErr> {
        if self.peek() == Some(&Tok::Ident("_".to_string())) {
            self.bump();
            return Ok(Pattern::Wild);
        }
        let ctor = self.ident()?;
        let binds = if self.peek() == Some(&Tok::LParen) {
            self.bump();
            let mut names = vec![self.ident()?];
            while self.peek() == Some(&Tok::Comma) {
                self.bump();
                names.push(self.ident()?);
            }
            self.eat(&Tok::RParen)?;
            names
        } else {
            Vec::new()
        };
        Ok(Pattern::Ctor(ctor, binds))
    }

    /// An atom, with `|` reset to bitwise-or for the duration: a delimited
    /// sub-expression (`(…)`, `[…]`, `{…}`, constructor args) inside a match-arm
    /// body uses `|` as bitwise, while the arm-level `|` (seen by `bitor` above)
    /// stays a separator.
    fn atom_base(&mut self) -> Result<Term, ParseErr> {
        let saved = std::mem::replace(&mut self.bar_is_arm, false);
        let r = self.atom_base_inner();
        self.bar_is_arm = saved;
        r
    }

    fn atom_base_inner(&mut self) -> Result<Term, ParseErr> {
        match self.peek() {
            // `{ x = e1, y = e2, … }` — a record literal.
            Some(Tok::LBrace) => {
                self.bump();
                let mut fields = Vec::new();
                while self.peek() != Some(&Tok::RBrace) {
                    let name = self.ident()?;
                    self.eat(&Tok::Eq)?;
                    let value = self.expr()?;
                    fields.push((name, value));
                    if self.peek() == Some(&Tok::Comma) {
                        self.bump();
                    } else {
                        break;
                    }
                }
                self.eat(&Tok::RBrace)?;
                Ok(Term::Record(fields))
            }
            // `[e1, e2, …, en]` — an array literal (at least one element, so the
            // element type is known). A *postfix* `[i]` (subscript) is handled in
            // `postfix`, so a leading `[` here is unambiguously a literal.
            Some(Tok::LBracket) => {
                self.bump();
                let mut elems = Vec::new();
                while self.peek() != Some(&Tok::RBracket) {
                    elems.push(self.expr()?);
                    if self.peek() == Some(&Tok::Comma) {
                        self.bump();
                    } else {
                        break;
                    }
                }
                self.eat(&Tok::RBracket)?;
                if elems.is_empty() {
                    return Err(self.err(
                        "an empty array literal `[]` has no element type — give at least one element"
                            .to_string(),
                    ));
                }
                Ok(Term::ArrayLit(elems))
            }
            Some(Tok::Int(n)) => {
                let n = *n;
                self.bump();
                Ok(Term::Int(n))
            }
            Some(Tok::Float(bits)) => {
                let bits = *bits;
                self.bump();
                Ok(Term::Float(bits))
            }
            Some(Tok::Str(s)) => {
                let s = s.clone();
                self.bump();
                Ok(Term::Str(s))
            }
            Some(Tok::LParen) => {
                self.bump();
                if self.peek() == Some(&Tok::RParen) {
                    self.bump();
                    Ok(Term::Unit)
                } else {
                    let first = self.expr()?;
                    if self.peek() == Some(&Tok::Comma) {
                        // `(e1, e2, …)` — a tuple.
                        let mut elems = vec![first];
                        while self.peek() == Some(&Tok::Comma) {
                            self.bump();
                            elems.push(self.expr()?);
                        }
                        self.eat(&Tok::RParen)?;
                        Ok(Term::Tuple(elems))
                    } else {
                        // `(e)` — just grouping.
                        self.eat(&Tok::RParen)?;
                        Ok(first)
                    }
                }
            }
            Some(Tok::DollarBrace) => {
                self.bump();
                let e = self.expr()?;
                self.eat(&Tok::RBrace)?;
                Ok(Term::Splice(Box::new(e)))
            }
            // `~b` — Bool negation sugar. It lowers to `if b then false else true`
            // so core Locus and effect inference stay unchanged.
            Some(Tok::Tilde) => {
                self.bump();
                let cond = self.atom()?;
                Ok(Term::If(
                    Box::new(cond),
                    Box::new(Term::Bool(false)),
                    Box::new(Term::Bool(true)),
                ))
            }
            // `!r` — dereference (read) a `Ref[T]` heap cell (`docs/mutability.md`
            // §1). A prefix `!` over an atom: `!r + 1` is `(!r) + 1`, `!a[i]` is
            // `!(a[i])`. Unambiguous in expression position — a type's latent-row
            // `!` only appears after `->` inside a *type*, never leading an
            // expression — so a leading `Tok::Bang` here is always a deref.
            Some(Tok::Bang) => {
                self.bump();
                let a = self.atom()?;
                Ok(Term::Deref(Box::new(a)))
            }
            Some(Tok::Ident(s)) => match s.as_str() {
                "true" => {
                    self.bump();
                    Ok(Term::Bool(true))
                }
                "false" => {
                    self.bump();
                    Ok(Term::Bool(false))
                }
                "perform" => {
                    self.bump();
                    let op = self.ident()?;
                    let arg = self.atom()?;
                    // Native names (console, fs, …) canonicalise to `World`.
                    Ok(Term::Perform(crate::prelude::op_label(&op), Box::new(arg)))
                }
                "quote" => {
                    self.bump();
                    self.eat(&Tok::LParen)?;
                    let e = self.expr()?;
                    self.eat(&Tok::RParen)?;
                    Ok(Term::Quote(Box::new(e)))
                }
                "genlet" => {
                    self.bump();
                    self.eat(&Tok::LParen)?;
                    let e = self.expr()?;
                    self.eat(&Tok::RParen)?;
                    Ok(Term::Genlet(Box::new(e)))
                }
                "letloc" => {
                    self.bump();
                    self.eat(&Tok::LBrace)?;
                    let e = self.expr()?;
                    self.eat(&Tok::RBrace)?;
                    Ok(Term::Letloc(Box::new(e)))
                }
                // `seal L { e }` — remove `L` from `e`'s row and forbid it
                // escaping the result (sealing-solution.md §4). The label name
                // canonicalises exactly as a written effect row does, so
                // `seal gc`, `seal mem`, `seal winapi`, and `seal myEffect` all
                // work.
                "seal" => {
                    self.bump();
                    let name = self.ident()?;
                    self.eat(&Tok::LBrace)?;
                    let e = self.expr()?;
                    self.eat(&Tok::RBrace)?;
                    Ok(Term::Seal(row_label(&name), Box::new(e)))
                }
                // `nogc { e }` ≝ `seal gc { e }` (sealing-solution.md §4): the GC
                // region. One construct, the gc label pinned.
                "nogc" => {
                    self.bump();
                    self.eat(&Tok::LBrace)?;
                    let e = self.expr()?;
                    self.eat(&Tok::RBrace)?;
                    Ok(Term::Seal(Label::Gc, Box::new(e)))
                }
                // The `mem` capability — raw memory primitives. Each takes its
                // operands as *atoms* (like `perform`), so a computed address is
                // parenthesised: `peek16 (base + i)`, `poke8 (buf + 1) 66`.
                "fill" => {
                    self.bump();
                    let dst = self.atom()?;
                    let byte = self.atom()?;
                    let count = self.atom()?;
                    Ok(Term::Fill(Box::new(dst), Box::new(byte), Box::new(count)))
                }
                "copy" => {
                    self.bump();
                    let dst = self.atom()?;
                    let src = self.atom()?;
                    let count = self.atom()?;
                    Ok(Term::Copy(Box::new(dst), Box::new(src), Box::new(count)))
                }
                // `len a` — the length of an array. Takes an atom (so `len a + 1`
                // is `(len a) + 1` and `len arr[0]` is `len (arr[0])`).
                "len" => {
                    self.bump();
                    let a = self.atom()?;
                    Ok(Term::Len(Box::new(a)))
                }
                // `ref e` — allocate a fresh `Ref[T]` heap cell holding `e`
                // (mutability; `docs/mutability.md` §1). A prefix operator over an
                // atom, exactly like `len`/`sqrt`: `ref e + 1` is `(ref e) + 1`,
                // `ref a[i]` is `ref (a[i])`. `ref` is a **contextual** keyword
                // (recognised only as an application head here), so it is not a stop
                // word and an identifier `ref` elsewhere is untouched.
                "ref" => {
                    self.bump();
                    let a = self.atom()?;
                    Ok(Term::RefNew(Box::new(a)))
                }
                "toFloat" => {
                    self.bump();
                    let a = self.atom()?;
                    Ok(Term::Cast(CastOp::ToFloat, Box::new(a)))
                }
                "floor" => {
                    self.bump();
                    let a = self.atom()?;
                    Ok(Term::Cast(CastOp::Floor, Box::new(a)))
                }
                "round" => {
                    self.bump();
                    let a = self.atom()?;
                    Ok(Term::Cast(CastOp::Round, Box::new(a)))
                }
                "toFloat32" => {
                    self.bump();
                    let a = self.atom()?;
                    Ok(Term::Cast(CastOp::ToFloat32, Box::new(a)))
                }
                "fromFloat32" => {
                    self.bump();
                    let a = self.atom()?;
                    Ok(Term::Cast(CastOp::FromFloat32, Box::new(a)))
                }
                "sqrt" => {
                    self.bump();
                    let a = self.atom()?;
                    Ok(Term::Sqrt(Box::new(a)))
                }
                "dot" if matches!(self.toks.get(self.pos + 1), Some((Tok::LParen, _))) => {
                    self.bump();
                    self.bump();
                    let a = self.expr()?;
                    self.eat(&Tok::Comma)?;
                    let b = self.expr()?;
                    self.eat(&Tok::RParen)?;
                    Ok(Term::Dot(Box::new(a), Box::new(b)))
                }
                "fma" => {
                    self.bump();
                    let (a, b, c) = if self.peek() == Some(&Tok::LParen) {
                        self.bump();
                        let a = self.expr()?;
                        self.eat(&Tok::Comma)?;
                        let b = self.expr()?;
                        self.eat(&Tok::Comma)?;
                        let c = self.expr()?;
                        self.eat(&Tok::RParen)?;
                        (a, b, c)
                    } else {
                        (self.atom()?, self.atom()?, self.atom()?)
                    };
                    Ok(Term::Fma(Box::new(a), Box::new(b), Box::new(c)))
                }
                "splatPair" | "splatQuad" | "splatOct" => {
                    let shape = VectorShape::from_splat_name(s).expect("matched splat name");
                    self.bump();
                    let a = self.atom()?;
                    Ok(Term::VectorSplat(shape, Box::new(a)))
                }
                // `loadPair`/`loadQuad`/`loadOct` (arr, i) — a packed array vector
                // load (SIMD Sprint 2), shaped like `dot(a, b)`: a parenthesised
                // argument pair `(array, element-index)`.
                "loadPair" | "loadQuad" | "loadOct" => {
                    let shape = VectorShape::from_load_name(s).expect("matched load name");
                    self.bump();
                    self.eat(&Tok::LParen)?;
                    let arr = self.expr()?;
                    self.eat(&Tok::Comma)?;
                    let idx = self.expr()?;
                    self.eat(&Tok::RParen)?;
                    Ok(Term::VectorLoad {
                        shape,
                        arr: Box::new(arr),
                        idx: Box::new(idx),
                    })
                }
                // `storePair`/`storeQuad`/`storeOct` (arr, i, v) — the matching
                // packed store, shaped like `fma(a, b, c)`.
                "storePair" | "storeQuad" | "storeOct" => {
                    let shape = VectorShape::from_store_name(s).expect("matched store name");
                    self.bump();
                    self.eat(&Tok::LParen)?;
                    let arr = self.expr()?;
                    self.eat(&Tok::Comma)?;
                    let idx = self.expr()?;
                    self.eat(&Tok::Comma)?;
                    let value = self.expr()?;
                    self.eat(&Tok::RParen)?;
                    Ok(Term::VectorStore {
                        shape,
                        arr: Box::new(arr),
                        idx: Box::new(idx),
                        value: Box::new(value),
                    })
                }
                _ => {
                    let name = s.clone();
                    self.bump();
                    if let Some(shape) = VectorShape::from_name(&name) {
                        let args = if self.peek() == Some(&Tok::LParen) {
                            self.bump();
                            let mut es = Vec::new();
                            if self.peek() != Some(&Tok::RParen) {
                                es.push(self.expr()?);
                                while self.peek() == Some(&Tok::Comma) {
                                    self.bump();
                                    es.push(self.expr()?);
                                }
                            }
                            self.eat(&Tok::RParen)?;
                            es
                        } else {
                            Vec::new()
                        };
                        return Ok(Term::VectorLit(shape, args));
                    }
                    // A **capitalised** identifier is a sum-type constructor:
                    // `C` (nullary) or `C(e1, …, en)`.
                    if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                        let args = if self.peek() == Some(&Tok::LParen) {
                            self.bump();
                            let mut es = Vec::new();
                            if self.peek() != Some(&Tok::RParen) {
                                es.push(self.expr()?);
                                while self.peek() == Some(&Tok::Comma) {
                                    self.bump();
                                    es.push(self.expr()?);
                                }
                            }
                            self.eat(&Tok::RParen)?;
                            es
                        } else {
                            Vec::new()
                        };
                        return Ok(Term::Construct(name, args));
                    }
                    // `peekW` / `pokeW` carry a bit-width suffix (`peek16`); a
                    // non-width suffix (or bare `peek`) is just a variable.
                    if let Some(w) = name.strip_prefix("peek").and_then(MemWidth::from_suffix) {
                        let addr = self.atom()?;
                        Ok(Term::Peek(w, Box::new(addr)))
                    } else if let Some(w) =
                        name.strip_prefix("poke").and_then(MemWidth::from_suffix)
                    {
                        let addr = self.atom()?;
                        let val = self.atom()?;
                        Ok(Term::Poke(w, Box::new(addr), Box::new(val)))
                    } else {
                        Ok(Term::Var(name))
                    }
                }
            },
            _ => Err(self.err(format!("expected an expression, found {:?}", self.peek()))),
        }
    }

    /// One operation declaration `op : Param -> Result` inside an `effect { … }`.
    fn op_decl(&mut self) -> Result<OpDecl, ParseErr> {
        let op = self.ident()?;
        self.eat(&Tok::Colon)?;
        let (param, result) = self.op_sig()?;
        Ok(OpDecl { op, param, result })
    }

    /// The `Param -> Result` of an operation signature (the latent row, if any,
    /// is ignored — an op's effect is the op itself).
    fn op_sig(&mut self) -> Result<(Type, Type), ParseErr> {
        match self.ty()? {
            Type::Fun(p, r, _) => Ok((*p, *r)),
            other => Err(self.err(format!(
                "an effect operation needs a `Param -> Result` type, found `{other}`"
            ))),
        }
    }

    /// Parse one **constraint** `Trait τ` (`trait-resolution.md` §1.1) — a trait
    /// name followed by a single type **atom** (so `Show a`, `Eq Int`, `Show
    /// List[a]` all parse; the atom keeps the constraint from greedily eating a
    /// following `=>` / `,`). The atom is the constrained type (D6 — one
    /// parameter). Used by the qualified-type grammar and the `requires …`
    /// clauses.
    fn constraint(&mut self) -> Result<Constraint, ParseErr> {
        let trait_name = self.ident()?;
        let ty = self.atom_ty()?;
        Ok(Constraint { trait_name, ty })
    }

    /// Parse a comma-separated **constraint list** `C1 a, C2 b, …` (one or more).
    /// Shared by the qualified-type prefix and `requires`.
    fn constraint_list(&mut self) -> Result<Vec<Constraint>, ParseErr> {
        let mut cs = vec![self.constraint()?];
        while self.peek() == Some(&Tok::Comma) {
            self.bump();
            cs.push(self.constraint()?);
        }
        Ok(cs)
    }

    /// Parse a **qualified type** `C a, D b => τ` (or a bare `τ`) — the
    /// constraints bind *outermost*, before the (right-associative) arrow type
    /// (`trait-resolution.md` §1.1). Returns the (possibly-empty) constraint list
    /// and the underlying type.
    ///
    /// **Disambiguation.** A leading constraint list is only a qualified type if a
    /// `=>` follows it; otherwise the input is an ordinary type. We try to parse a
    /// constraint list and peek for `=>`; if it is not there, we rewind and parse
    /// a plain `ty()`. (A constraint atom is `ident atom_ty`, which a plain type
    /// `ident` is a prefix of, so the rewind is what keeps `Int`, `List[a]`, etc.
    /// parsing unchanged.)
    pub(crate) fn qualified_ty(&mut self) -> Result<(Vec<Constraint>, Type), ParseErr> {
        let save = self.pos;
        if matches!(self.peek(), Some(Tok::Ident(_))) {
            if let Ok(cs) = self.constraint_list() {
                if self.peek() == Some(&Tok::FatArrow) {
                    self.bump();
                    let t = self.ty()?;
                    return Ok((cs, t));
                }
            }
            // Not a qualified type — rewind and parse a plain type.
            self.pos = save;
        }
        Ok((Vec::new(), self.ty()?))
    }

    fn ty(&mut self) -> Result<Type, ParseErr> {
        let t = self.atom_ty()?;
        if self.peek() == Some(&Tok::Arrow) {
            self.bump();
            let u = self.ty()?; // `->` is right-associative
                                // An optional latent effect row on THIS arrow: `A -> B ! {m, ...}`.
                                // The recursive `ty()` already consumed any row on inner arrows, so a
                                // `!` reaching here belongs to the arrow we are about to build — which
                                // (right-associativity) is the innermost one, where the call fires.
            let row = if self.peek() == Some(&Tok::Bang) {
                self.bump();
                self.row()?
            } else {
                Row::pure()
            };
            Ok(Type::Fun(Box::new(t), Box::new(u), row))
        } else {
            Ok(t)
        }
    }

    /// An effect row `{}` / `{ l }` / `{ l, m, … }` / `{ l | r, s }` — the
    /// labels after a `!`, optionally followed by one or more row-variable tails.
    fn row(&mut self) -> Result<Row, ParseErr> {
        self.eat(&Tok::LBrace)?;
        let mut labels = BTreeSet::new();
        let mut tails = BTreeSet::new();
        while self.peek() != Some(&Tok::RBrace) {
            match self.peek() {
                Some(Tok::Bar) => {
                    self.bump();
                    loop {
                        let name = self.ident()?;
                        tails.insert(self.row_var(name));
                        match self.peek() {
                            Some(Tok::Comma) => self.bump(),
                            Some(Tok::RBrace) => break,
                            other => {
                                return Err(self.err(format!(
                                    "expected `,` or `}}` in row-variable tail, found {other:?}"
                                )));
                            }
                        }
                    }
                    break;
                }
                Some(Tok::Ident(_)) => {
                    labels.insert(self.effect_label()?);
                    match self.peek() {
                        Some(Tok::Comma) => self.bump(),
                        Some(Tok::Bar) | Some(Tok::RBrace) => {}
                        other => {
                            return Err(self.err(format!(
                                "expected `,`, `|`, or `}}` in effect row, found {other:?}"
                            )));
                        }
                    }
                }
                other => {
                    return Err(self.err(format!(
                        "expected an effect label or row variable tail, found {other:?}"
                    )));
                }
            }
        }
        self.eat(&Tok::RBrace)?;
        Ok(Row::with_tails(labels, tails))
    }

    fn atom_ty(&mut self) -> Result<Type, ParseErr> {
        match self.peek() {
            Some(Tok::LParen) => {
                self.bump();
                let first = self.ty()?;
                if self.peek() == Some(&Tok::Comma) {
                    // `(A, B, …)` — a tuple type.
                    let mut elems = vec![first];
                    while self.peek() == Some(&Tok::Comma) {
                        self.bump();
                        elems.push(self.ty()?);
                    }
                    self.eat(&Tok::RParen)?;
                    Ok(Type::Tuple(elems))
                } else {
                    self.eat(&Tok::RParen)?;
                    Ok(first)
                }
            }
            // `{ x: A, y: B, … }` — a record type.
            Some(Tok::LBrace) => {
                self.bump();
                let mut fields = Vec::new();
                while self.peek() != Some(&Tok::RBrace) {
                    let name = self.ident()?;
                    self.eat(&Tok::Colon)?;
                    let ty = self.ty()?;
                    fields.push((name, ty));
                    if self.peek() == Some(&Tok::Comma) {
                        self.bump();
                    } else {
                        break;
                    }
                }
                self.eat(&Tok::RBrace)?;
                Ok(Type::Record(fields))
            }
            Some(Tok::Ident(s)) => match s.as_str() {
                "Int" => {
                    self.bump();
                    Ok(Type::Int)
                }
                "Float" => {
                    self.bump();
                    Ok(Type::Float)
                }
                "Float32" => {
                    self.bump();
                    Ok(Type::Float32)
                }
                "Bool" => {
                    self.bump();
                    Ok(Type::Bool)
                }
                "Unit" => {
                    self.bump();
                    Ok(Type::Unit)
                }
                "String" => {
                    self.bump();
                    Ok(Type::Str)
                }
                "I32" => {
                    self.bump();
                    Ok(Type::I32)
                }
                "U32" => {
                    self.bump();
                    Ok(Type::U32)
                }
                "Ptr" => {
                    self.bump();
                    Ok(Type::Ptr)
                }
                "Code" => {
                    self.bump();
                    self.eat(&Tok::LBracket)?;
                    let t = self.ty()?;
                    self.eat(&Tok::RBracket)?;
                    Ok(Type::Code(Box::new(t), Row::pure()))
                }
                "Array" => {
                    self.bump();
                    self.eat(&Tok::LBracket)?;
                    let t = self.ty()?;
                    self.eat(&Tok::RBracket)?;
                    Ok(Type::Array(Box::new(t)))
                }
                "Pair" | "Quad" | "Oct" => {
                    let shape = VectorShape::from_name(s).expect("matched vector type name");
                    self.bump();
                    self.eat(&Tok::LBracket)?;
                    let t = self.ty()?;
                    self.eat(&Tok::RBracket)?;
                    Ok(Type::Vector(shape, Box::new(t)))
                }
                "Mask" => {
                    self.bump();
                    self.eat(&Tok::LBracket)?;
                    let shape_name = self.ident()?;
                    let Some(shape) = VectorShape::from_name(&shape_name) else {
                        return Err(self.err(format!(
                            "expected Pair, Quad, or Oct in Mask[...], found {shape_name}"
                        )));
                    };
                    self.eat(&Tok::RBracket)?;
                    Ok(Type::Mask(shape))
                }
                "Pairs" | "Quads" | "Octs" => {
                    let shape =
                        VectorShape::from_plural_name(s).expect("matched vector-array type name");
                    self.bump();
                    self.eat(&Tok::LBracket)?;
                    let t = self.ty()?;
                    self.eat(&Tok::RBracket)?;
                    Ok(Type::Array(Box::new(Type::Vector(shape, Box::new(t)))))
                }
                // Any other identifier is a nominal reference to a `type`-declared
                // sum (e.g. `List`, `Option`) — with an **optional** `[T, …]`
                // type-argument list (`List[Int]`, `Pair[Int, Bool]`). Reuses the
                // same bracket grammar as `Array[T]` (which keeps its dedicated
                // arm above, as does `Code`). A bare `a` is `Named("a", [])` — a
                // monomorphic sum, or a type parameter (sema disambiguates).
                // Whether it's actually declared (and named with the right arity)
                // is checked by sema when a value of the type is used.
                other => {
                    let name = other.to_string();
                    self.bump();
                    let args = if self.peek() == Some(&Tok::LBracket) {
                        self.bump();
                        let mut ts = vec![self.ty()?];
                        while self.peek() == Some(&Tok::Comma) {
                            self.bump();
                            ts.push(self.ty()?);
                        }
                        self.eat(&Tok::RBracket)?;
                        ts
                    } else {
                        Vec::new()
                    };
                    Ok(Type::Named(name, args))
                }
            },
            _ => Err(self.err(format!("expected a type, found {:?}", self.peek()))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::{infer, Ctx, Sig};
    use crate::syntax::{BinOp, CastOp, Label, Row, Term, Type, VectorShape};

    fn user(s: &str) -> Label {
        Label::User(s.to_string())
    }
    fn world(s: &str) -> Label {
        Label::World(s.to_string())
    }

    // ── the AST a snippet parses to ─────────────────────────────────────

    #[test]
    fn literals_and_unit() {
        assert_eq!(parse("42").unwrap(), Term::Int(42));
        assert_eq!(parse("1.25").unwrap(), Term::Float(1.25f64.to_bits()));
        assert_eq!(parse("1.0e-3").unwrap(), Term::Float(1.0e-3f64.to_bits()));
        assert_eq!(parse("true").unwrap(), Term::Bool(true));
        assert_eq!(parse("()").unwrap(), Term::Unit);
    }

    #[test]
    fn line_comments_are_skipped() {
        // `--` runs to end of line; code resumes after. The comment holds a
        // comma and a stray `-` (exactly what first tripped the lexer).
        let t = parse("-- header, with a comma and a -dash\n1 + 2  -- trailing\n").unwrap();
        assert_eq!(
            t,
            Term::Bin(BinOp::Add, Box::new(Term::Int(1)), Box::new(Term::Int(2)))
        );
    }

    #[test]
    fn arithmetic_binds_by_precedence() {
        // 1 + 2 * 3  parses as  1 + (2 * 3)
        assert_eq!(
            parse("1 + 2 * 3").unwrap(),
            Term::Bin(
                BinOp::Add,
                Box::new(Term::Int(1)),
                Box::new(Term::Bin(
                    BinOp::Mul,
                    Box::new(Term::Int(2)),
                    Box::new(Term::Int(3))
                )),
            )
        );
        // application binds tighter than `*`: `f x * 2` = `(f x) * 2`
        assert!(matches!(
            parse("f x * 2").unwrap(),
            Term::Bin(BinOp::Mul, _, _)
        ));
        assert!(matches!(
            parse("1.0 / 2.0").unwrap(),
            Term::Bin(BinOp::Div, _, _)
        ));
        assert!(matches!(
            parse("toFloat 1").unwrap(),
            Term::Cast(CastOp::ToFloat, _)
        ));
        assert!(matches!(
            parse("fromFloat32 (toFloat32 1.0)").unwrap(),
            Term::Cast(CastOp::FromFloat32, _)
        ));
        // comparison yields a Bin (non-chaining)
        assert!(matches!(
            parse("a < b").unwrap(),
            Term::Bin(BinOp::Lt, _, _)
        ));
        assert!(matches!(
            parse("a <= b").unwrap(),
            Term::Bin(BinOp::Le, _, _)
        ));
        assert!(matches!(
            parse("a > b").unwrap(),
            Term::Bin(BinOp::Gt, _, _)
        ));
        assert!(matches!(
            parse("a >= b").unwrap(),
            Term::Bin(BinOp::Ge, _, _)
        ));
        assert!(matches!(
            parse("a != b").unwrap(),
            Term::Bin(BinOp::Ne, _, _)
        ));
    }

    #[test]
    fn accumulator_loop_parses() {
        let t = parse("loop i = 0, acc = 0 while i < 10 do i + 1, acc + i else acc").unwrap();
        let Term::Loop {
            vars,
            cond,
            steps,
            result,
        } = t
        else {
            panic!("expected loop")
        };
        assert_eq!(vars.len(), 2);
        assert_eq!(vars[0].0, "i");
        assert_eq!(vars[1].0, "acc");
        assert!(matches!(*cond, Term::Bin(BinOp::Lt, _, _)));
        assert_eq!(steps.len(), 2);
        assert!(matches!(*result, Term::Var(ref x) if x == "acc"));
    }

    #[test]
    fn endloop_parses_as_unit_loop_result() {
        let t = parse("loop i = 0 while i < 10 do i + 1 endloop").unwrap();
        let Term::Loop { steps, result, .. } = t else {
            panic!("expected loop")
        };
        assert_eq!(steps.len(), 1);
        assert_eq!(*result, Term::Unit);
    }

    #[test]
    fn loop_return_parses_as_loop_result() {
        let t = parse("loop i = 0, acc = 0 while i < 10 do i + 1, acc + i return acc").unwrap();
        let Term::Loop { steps, result, .. } = t else {
            panic!("expected loop")
        };
        assert_eq!(steps.len(), 2);
        assert!(matches!(*result, Term::Var(ref x) if x == "acc"));
    }

    #[test]
    fn case_sugar_desugars_to_let_and_ifs() {
        let t = parse("case x + 1 of | 1 => 10 | 2 => 20 | _ => 30").unwrap();
        let Term::Let(tmp, scrutinee, body) = t else {
            panic!("case sugar should desugar to a let-bound scrutinee")
        };
        assert!(
            tmp.starts_with('\u{1}'),
            "generated binder should be hidden"
        );
        assert!(matches!(*scrutinee, Term::Bin(BinOp::Add, _, _)));

        let Term::If(cond1, then1, else1) = *body else {
            panic!("case sugar should desugar to nested ifs")
        };
        assert_eq!(*then1, Term::Int(10));
        match *cond1 {
            Term::Bin(BinOp::Eq, lhs, rhs) => {
                assert_eq!(*lhs, Term::Var(tmp.clone()));
                assert_eq!(*rhs, Term::Int(1));
            }
            other => panic!("first case condition should be equality, got {other:?}"),
        }

        let Term::If(cond2, then2, else2) = *else1 else {
            panic!("second case arm should be the nested else")
        };
        assert_eq!(*then2, Term::Int(20));
        assert_eq!(*else2, Term::Int(30));
        match *cond2 {
            Term::Bin(BinOp::Eq, lhs, rhs) => {
                assert_eq!(*lhs, Term::Var(tmp));
                assert_eq!(*rhs, Term::Int(2));
            }
            other => panic!("second case condition should be equality, got {other:?}"),
        }
    }

    #[test]
    fn cond_sugar_desugars_to_ifs() {
        let t = parse("cond | x < 0 => 1 | x == 0 => 2 | _ => 3").unwrap();
        let Term::If(cond1, then1, else1) = t else {
            panic!("cond sugar should desugar to nested ifs")
        };
        assert!(matches!(*cond1, Term::Bin(BinOp::Lt, _, _)));
        assert_eq!(*then1, Term::Int(1));
        let Term::If(cond2, then2, else2) = *else1 else {
            panic!("second cond arm should be the nested else")
        };
        assert!(matches!(*cond2, Term::Bin(BinOp::Eq, _, _)));
        assert_eq!(*then2, Term::Int(2));
        assert_eq!(*else2, Term::Int(3));
    }

    #[test]
    fn case_and_cond_require_final_defaults() {
        let err = parse("case x of | 1 => 2").unwrap_err();
        assert!(err.msg.contains("default"), "got: {}", err.msg);
        let err = parse("cond | true => 1").unwrap_err();
        assert!(err.msg.contains("default"), "got: {}", err.msg);
        let err = parse("case x of | _ => 1 | 2 => 3").unwrap_err();
        assert!(err.msg.contains("must be last"), "got: {}", err.msg);
    }

    #[test]
    fn do_sugar_desugars_to_nested_lets() {
        let t = parse("do { let x = 1; let y = x + 1; y }").unwrap();
        let Term::Let(x, x_value, body) = t else {
            panic!("first do binding should become a let")
        };
        assert_eq!(x, "x");
        assert_eq!(*x_value, Term::Int(1));

        let Term::Let(y, y_value, body) = *body else {
            panic!("second do binding should become a nested let")
        };
        assert_eq!(y, "y");
        assert!(matches!(*y_value, Term::Bin(BinOp::Add, _, _)));
        assert_eq!(*body, Term::Var("y".into()));
    }

    #[test]
    fn do_expression_statements_get_hidden_binders() {
        let t = parse("do { 1 + 2; 4 }").unwrap();
        let Term::Let(tmp, value, body) = t else {
            panic!("do expression statement should become a hidden let")
        };
        assert!(tmp.starts_with('\u{1}'));
        assert!(matches!(*value, Term::Bin(BinOp::Add, _, _)));
        assert_eq!(*body, Term::Int(4));
    }

    #[test]
    fn empty_do_block_is_unit() {
        assert_eq!(parse("do {}").unwrap(), Term::Unit);
        assert_eq!(
            parse("do { let x = 1; }").unwrap(),
            Term::Let("x".into(), Box::new(Term::Int(1)), Box::new(Term::Unit))
        );
    }

    #[test]
    fn overflow_operator_spellings_parse() {
        assert!(matches!(
            parse("1 +% 2").unwrap(),
            Term::Bin(BinOp::AddWrap, _, _)
        ));
        assert!(matches!(
            parse("1 -? 2").unwrap(),
            Term::Bin(BinOp::SubChecked, _, _)
        ));
        assert!(matches!(
            parse("2 *% 3 *? 4").unwrap(),
            Term::Bin(BinOp::MulChecked, _, _)
        ));
        assert!(matches!(
            parse("1 +? 2 *% 3").unwrap(),
            Term::Bin(BinOp::AddChecked, _, _)
        ));
    }

    #[test]
    fn modulo_operator_parses_at_multiplicative_precedence() {
        assert!(matches!(
            parse("7 % 3").unwrap(),
            Term::Bin(BinOp::Mod, _, _)
        ));
        assert!(matches!(
            parse("1 + 7 % 3").unwrap(),
            Term::Bin(BinOp::Add, _, _)
        ));
        assert!(matches!(
            parse("7 % 3 * 2").unwrap(),
            Term::Bin(BinOp::Mul, _, _)
        ));
    }

    #[test]
    fn logical_connectives_parse_as_short_circuit_sugar() {
        let and = parse("a && b").unwrap();
        assert!(matches!(
            and,
            Term::If(_, _, ref else_branch) if **else_branch == Term::Bool(false)
        ));

        let or = parse("a || b").unwrap();
        assert!(matches!(
            or,
            Term::If(_, ref then_branch, _) if **then_branch == Term::Bool(true)
        ));

        let cmp_and = parse("a < b && c < d").unwrap();
        assert!(matches!(
            cmp_and,
            Term::If(ref cond, _, _) if matches!(**cond, Term::Bin(BinOp::Lt, _, _))
        ));
    }

    #[test]
    fn tilde_parses_as_bool_negation_sugar() {
        let not = parse("~flag").unwrap();
        assert!(matches!(
            not,
            Term::If(_, ref then_branch, ref else_branch)
                if **then_branch == Term::Bool(false) && **else_branch == Term::Bool(true)
        ));

        let combined = parse("~flag && other").unwrap();
        assert!(matches!(
            combined,
            Term::If(ref cond, _, ref else_branch)
                if matches!(**cond, Term::If(_, _, _)) && **else_branch == Term::Bool(false)
        ));
    }

    #[test]
    fn hex_literals_and_bitwise_precedence() {
        // hex decodes to its value (the UTF-8/byte-mask spelling).
        assert_eq!(parse("0xFF").unwrap(), Term::Int(255));
        assert_eq!(parse("0x10FFFF").unwrap(), Term::Int(0x10_FFFF));
        // bitwise binds LOOSER than arithmetic: `a + b & c` = `(a + b) & c`.
        assert!(matches!(
            parse("a + b & c").unwrap(),
            Term::Bin(BinOp::And, _, _)
        ));
        // comparison is LOOSER than bitwise — NOT C's footgun: `a & b == c`
        // is `(a & b) == c`, so the top node is `==`.
        assert!(matches!(
            parse("a & b == c").unwrap(),
            Term::Bin(BinOp::Eq, _, _)
        ));
        // `|` is looser than `&`: `a | b & c` = `a | (b & c)`.
        assert!(matches!(
            parse("a | b & c").unwrap(),
            Term::Bin(BinOp::Or, _, _)
        ));
        // a shift binds tighter than `+`: `x << 2 + 1` = `(x << 2) + 1`.
        assert!(matches!(
            parse("x << 2 + 1").unwrap(),
            Term::Bin(BinOp::Add, _, _)
        ));
    }

    #[test]
    fn array_accessor_parses() {
        // read and store forms
        assert!(matches!(parse("a[0]").unwrap(), Term::Index(..)));
        assert!(matches!(parse("a[0] <- 65").unwrap(), Term::IndexSet(..)));
        // the index argument is parenthesised in application: `f a[i]` = `f (a[i])`
        match parse("f a[i]").unwrap() {
            Term::App(_, arg) => assert!(matches!(*arg, Term::Index(..)), "arg is the index"),
            other => panic!("expected an application, got {other:?}"),
        }
        // subscripts chain: `a[i][j]` = `(a[i])[j]`
        match parse("a[i][j]").unwrap() {
            Term::Index(base, _) => assert!(matches!(*base, Term::Index(..)), "base is a[i]"),
            other => panic!("expected an index, got {other:?}"),
        }
        // the store value reaches down to bitwise: `out[j] <- 0x80 | b`
        match parse("out[j] <- 0x80 | b").unwrap() {
            Term::IndexSet(_, _, v) => assert!(matches!(*v, Term::Bin(BinOp::Or, _, _))),
            other => panic!("expected a store, got {other:?}"),
        }
    }

    #[test]
    fn mutability_surface_parses() {
        // mutability v1 (`docs/mutability-sprints.md`): `let mut` binds a mutable
        // local, `x := e` assigns it. Surface only in Sprint 1 — these parse to AST.
        match parse("let mut x = 1 in x").unwrap() {
            Term::LetMut(name, init, body) => {
                assert_eq!(name, "x");
                assert_eq!(*init, Term::Int(1));
                assert_eq!(*body, Term::Var("x".into()));
            }
            other => panic!("expected a `let mut`, got {other:?}"),
        }
        // `x := 2` is the assignment expression.
        match parse("x := 2").unwrap() {
            Term::Assign(name, value) => {
                assert_eq!(name, "x");
                assert_eq!(*value, Term::Int(2));
            }
            other => panic!("expected an assignment, got {other:?}"),
        }
        // `:=` is low-precedence: the value runs down to arithmetic.
        match parse("x := x + 41").unwrap() {
            Term::Assign(name, value) => {
                assert_eq!(name, "x");
                assert!(matches!(*value, Term::Bin(BinOp::Add, _, _)));
            }
            other => panic!("expected an assignment, got {other:?}"),
        }
        // a `let mut` body can hold an assignment (the `factIter`-style use):
        // `let mut x = 1 in let _ = (x := 2) in x`.
        match parse("let mut x = 1 in let _ = (x := 2) in x").unwrap() {
            Term::LetMut(_, _, body) => match *body {
                Term::Let(_, rhs, _) => assert!(matches!(*rhs, Term::Assign(..))),
                other => panic!("expected an inner `let`, got {other:?}"),
            },
            other => panic!("expected a `let mut`, got {other:?}"),
        }
        // `mut` is contextual: a plain `let` and a bare variable are unaffected.
        assert!(matches!(parse("let x = 1 in x").unwrap(), Term::Let(..)));
        assert_eq!(parse("x").unwrap(), Term::Var("x".into()));
    }

    #[test]
    fn ref_operators_parse() {
        // `ref e` — a prefix operator over an atom (like `len`/`sqrt`).
        match parse("ref 0").unwrap() {
            Term::RefNew(e) => assert_eq!(*e, Term::Int(0)),
            other => panic!("expected `ref`, got {other:?}"),
        }
        // `!r` — a prefix deref over an atom.
        match parse("!r").unwrap() {
            Term::Deref(e) => assert_eq!(*e, Term::Var("r".into())),
            other => panic!("expected a deref, got {other:?}"),
        }
        // `!r + 41` is `(!r) + 41` — deref binds tighter than `+`.
        match parse("!r + 41").unwrap() {
            Term::Bin(BinOp::Add, lhs, rhs) => {
                assert!(matches!(*lhs, Term::Deref(_)), "lhs is a deref");
                assert_eq!(*rhs, Term::Int(41));
            }
            other => panic!("expected an add of a deref, got {other:?}"),
        }
        // `ref e + 1` is `(ref e) + 1` — `ref` binds tighter than `+`, like `len`.
        match parse("ref 0 + 1").unwrap() {
            Term::Bin(BinOp::Add, lhs, _) => assert!(matches!(*lhs, Term::RefNew(_))),
            other => panic!("expected an add of a ref, got {other:?}"),
        }
        // `r := !r + 41` — a bare-name assignment whose value derefs `r` (the gate
        // shape). The surface is `Term::Assign`; sema splits it to the heap write.
        match parse("r := !r + 41").unwrap() {
            Term::Assign(name, value) => {
                assert_eq!(name, "r");
                assert!(matches!(*value, Term::Bin(BinOp::Add, _, _)));
            }
            other => panic!("expected an assignment, got {other:?}"),
        }
        // `ref` / a leading `!` are contextual: a bare `ref`/identifier is untouched
        // (no program names `ref`, but the parser must not reserve it as a stop word).
        assert_eq!(parse("r").unwrap(), Term::Var("r".into()));
        // `!r` as an application argument: `f !r` = `f (!r)`.
        match parse("f !r").unwrap() {
            Term::App(_, arg) => assert!(matches!(*arg, Term::Deref(_))),
            other => panic!("expected an application, got {other:?}"),
        }
    }

    #[test]
    fn colon_eq_lexes_as_one_token() {
        // `:=` is a single token, distinct from `:` (which stays `Tok::Colon`).
        let toks: Vec<Tok> = tokenize("x := 2")
            .unwrap()
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        assert_eq!(
            toks,
            vec![Tok::Ident("x".into()), Tok::ColonEq, Tok::Int(2)]
        );
        // a lone `:` is still `Tok::Colon` (e.g. a `let rec`/`fn` annotation colon).
        let colon: Vec<Tok> = tokenize("x : Int")
            .unwrap()
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        assert_eq!(
            colon,
            vec![Tok::Ident("x".into()), Tok::Colon, Tok::Ident("Int".into())]
        );
    }

    #[test]
    fn effect_rows_parse_on_arrows() {
        // `A -> B ! {mem}` attaches the latent row to the (innermost) arrow — the
        // signature surface for effectful functions, e.g. an effectful `let rec`.
        match parse("fn g: Int -> Int ! {mem} => g").unwrap() {
            Term::Lam(_, Some(Type::Fun(_, _, row)), _) => {
                assert_eq!(row, Row::single(world("mem")), "{{mem}} on the arrow");
            }
            other => panic!("expected an effectful param type, got {other:?}"),
        }
        // multiple labels, comma-separated as they print.
        match parse("fn g: Int -> Int ! {mem, winapi, exn[Overflow]} => g").unwrap() {
            Term::Lam(_, Some(Type::Fun(_, _, row)), _) => {
                let want = Row::single(world("mem"))
                    .union(&Row::single(world("winapi")))
                    .union(&Row::single(Label::Exn("Overflow".into())));
                assert_eq!(row, want, "labels include parameterized exn");
            }
            other => panic!("expected labels, got {other:?}"),
        }
        // a pure arrow is still pure (no `!`).
        match parse("fn g: Int -> Int => g").unwrap() {
            Term::Lam(_, Some(Type::Fun(_, _, row)), _) => assert!(row.is_pure()),
            other => panic!("expected a pure arrow, got {other:?}"),
        }
        // open rows let annotations name a callback's residual effects and pass
        // that same row tail outward.
        match parse("fn f: (Int -> Int ! {| r}) -> Int ! {gc | r} => f").unwrap() {
            Term::Lam(_, Some(Type::Fun(arg, _, outer_row)), _) => match *arg {
                Type::Fun(_, _, cb_row) => {
                    assert!(cb_row
                        .tail_set()
                        .iter()
                        .any(|id| id.parsed_index().is_some()));
                    assert_eq!(
                        cb_row.tail_set(),
                        outer_row.tail_set(),
                        "same row variable name"
                    );
                    assert!(outer_row.labels().any(|l| *l == Label::Gc));
                }
                other => panic!("expected a callback arrow, got {other:?}"),
            },
            other => panic!("expected an open-row annotation, got {other:?}"),
        }

        match parse("fn f: (Int -> Int ! {| r}) -> Int ! {| r, s} => 1").unwrap() {
            Term::Lam(_, Some(Type::Fun(arg, _, outer_row)), _) => {
                assert_eq!(
                    outer_row.tail_set().len(),
                    2,
                    "multi-tail rows preserve both tails"
                );
                match *arg {
                    Type::Fun(_, _, cb_row) => {
                        assert_eq!(cb_row.tail_set().len(), 1);
                        assert!(cb_row.tail_set().is_subset(outer_row.tail_set()));
                    }
                    other => panic!("expected a callback arrow, got {other:?}"),
                }
            }
            other => panic!("expected a multi-tail row annotation, got {other:?}"),
        }
    }

    #[test]
    fn tuples_parse() {
        // `(e1, e2, …)` is a tuple; `(e)` is grouping; `()` is Unit.
        assert!(matches!(parse("(1, 2)").unwrap(), Term::Tuple(es) if es.len() == 2));
        assert!(matches!(parse("(1, 2, 3)").unwrap(), Term::Tuple(es) if es.len() == 3));
        assert!(
            matches!(parse("(42)").unwrap(), Term::Int(42)),
            "(e) is grouping"
        );
        assert_eq!(parse("()").unwrap(), Term::Unit);
        // `let (x, y) = e in body` destructures.
        match parse("let (x, y) = p in x").unwrap() {
            Term::LetTuple(names, _, _) => {
                assert_eq!(names, vec!["x".to_string(), "y".to_string()])
            }
            other => panic!("expected a let-tuple, got {other:?}"),
        }
        // tuple TYPE in a signature.
        match parse("fn f: (Int, Bool) => f").unwrap() {
            Term::Lam(_, Some(Type::Tuple(ts)), _) => assert_eq!(ts.len(), 2),
            other => panic!("expected a tuple param type, got {other:?}"),
        }
    }

    #[test]
    fn records_parse() {
        // `{ x = e, … }` is a record literal.
        assert!(matches!(parse("{ x = 1, y = 2 }").unwrap(), Term::Record(fs) if fs.len() == 2));
        // `r.x.y` is chained field access.
        match parse("r.x.y").unwrap() {
            Term::Field(inner, f) => {
                assert_eq!(f, "y");
                assert!(matches!(*inner, Term::Field(_, _)), "chains");
            }
            other => panic!("expected field access, got {other:?}"),
        }
        // record TYPE in a signature.
        match parse("fn f: { x: Int, y: Bool } => f").unwrap() {
            Term::Lam(_, Some(Type::Record(fs)), _) => assert_eq!(fs.len(), 2),
            other => panic!("expected a record param type, got {other:?}"),
        }
    }

    #[test]
    fn effect_declarations_parse() {
        // multi-op: `effect Name { op : P -> R ; … }` → one OpDecl per operation.
        match parse("effect State { get : Unit -> Int ; put : Int -> Unit } in 0").unwrap() {
            Term::Effect { name, ops, .. } => {
                assert_eq!(name, "State");
                assert_eq!(ops.len(), 2);
                assert_eq!(ops[0].op, "get");
                assert_eq!(ops[0].result, Type::Int);
                assert_eq!(ops[1].op, "put");
                assert_eq!(ops[1].param, Type::Int);
            }
            other => panic!("expected an effect declaration, got {other:?}"),
        }
        // single-op `effect ask : …` still works — one op named for the effect.
        match parse("effect ask : Unit -> Int in 0").unwrap() {
            Term::Effect { name, ops, .. } => {
                assert_eq!(name, "ask");
                assert_eq!(ops.len(), 1);
                assert_eq!(ops[0].op, "ask");
            }
            other => panic!("expected an effect declaration, got {other:?}"),
        }
    }

    #[test]
    fn string_literals_and_escapes() {
        assert_eq!(parse(r#""hello""#).unwrap(), Term::Str("hello".into()));
        // `\n` and `\"` decode; the result holds a real newline and quote.
        assert_eq!(parse(r#""a\nb\"c""#).unwrap(), Term::Str("a\nb\"c".into()));
        // `String` is a type.
        assert_eq!(
            parse("fn s: String => s").unwrap(),
            Term::Lam("s".into(), Some(Type::Str), Box::new(Term::Var("s".into())))
        );
        assert_eq!(
            parse("fn x: Float => x").unwrap(),
            Term::Lam(
                "x".into(),
                Some(Type::Float),
                Box::new(Term::Var("x".into()))
            )
        );
        assert_eq!(
            parse("fn x: Float32 => x").unwrap(),
            Term::Lam(
                "x".into(),
                Some(Type::Float32),
                Box::new(Term::Var("x".into()))
            )
        );
    }

    #[test]
    fn vector_types_literals_and_splats_parse() {
        match parse("fn v: Quad[Float32] => v").unwrap() {
            Term::Lam(_, Some(Type::Vector(VectorShape::Quad, elem)), _) => {
                assert_eq!(*elem, Type::Float32)
            }
            other => panic!("expected a vector parameter annotation, got {other:?}"),
        }

        match parse("fn xs: Quads[Float32] => xs").unwrap() {
            Term::Lam(_, Some(Type::Array(elem)), _) => match *elem {
                Type::Vector(VectorShape::Quad, lane) => assert_eq!(*lane, Type::Float32),
                other => panic!("expected a Quad array element, got {other:?}"),
            },
            other => panic!("expected a vector-array parameter annotation, got {other:?}"),
        }

        match parse("fn xs: Pairs[Float] => xs").unwrap() {
            Term::Lam(_, Some(Type::Array(elem)), _) => match *elem {
                Type::Vector(VectorShape::Pair, lane) => assert_eq!(*lane, Type::Float),
                other => panic!("expected a Pair array element, got {other:?}"),
            },
            other => panic!("expected a vector-array parameter annotation, got {other:?}"),
        }

        match parse("fn m: Mask[Quad] => m").unwrap() {
            Term::Lam(_, Some(Type::Mask(VectorShape::Quad)), _) => {}
            other => panic!("expected a mask parameter annotation, got {other:?}"),
        }

        assert!(matches!(
            parse(
                "Quad(toFloat32 1.0, toFloat32 2.0, toFloat32 3.0, toFloat32 4.0)"
            )
            .unwrap(),
            Term::VectorLit(VectorShape::Quad, elems) if elems.len() == 4
        ));
        assert!(matches!(
            parse("splatQuad (toFloat32 1.0)").unwrap(),
            Term::VectorSplat(VectorShape::Quad, _)
        ));
        assert!(matches!(
            parse("sqrt (toFloat32 4.0)").unwrap(),
            Term::Sqrt(_)
        ));
        assert!(matches!(
            parse("sum (splatQuad (toFloat32 1.0))").unwrap(),
            Term::App(..)
        ));
        assert!(matches!(
            parse("length (splatQuad (toFloat32 1.0))").unwrap(),
            Term::App(..)
        ));
        assert!(matches!(
            parse("dot(splatQuad (toFloat32 1.0), splatQuad (toFloat32 2.0))").unwrap(),
            Term::Dot(_, _)
        ));
        assert!(matches!(
            parse(
                "select(splatQuad (toFloat32 1.0) < splatQuad (toFloat32 2.0), \
                 splatQuad (toFloat32 1.0), splatQuad (toFloat32 2.0))"
            )
            .unwrap(),
            Term::App(..)
        ));
        assert!(matches!(
            parse("fma(splatQuad (toFloat32 2.0), splatQuad (toFloat32 3.0), splatQuad (toFloat32 4.0))").unwrap(),
            Term::Fma(_, _, _)
        ));
    }

    #[test]
    fn lambda_application_and_let() {
        assert_eq!(
            parse("fn x: Int => x").unwrap(),
            Term::Lam("x".into(), Some(Type::Int), Box::new(Term::Var("x".into())))
        );
        assert_eq!(
            parse("f a b").unwrap(),
            Term::App(
                Box::new(Term::App(
                    Box::new(Term::Var("f".into())),
                    Box::new(Term::Var("a".into()))
                )),
                Box::new(Term::Var("b".into()))
            )
        );
        assert_eq!(
            parse("let y = 1 in y").unwrap(),
            Term::Let(
                "y".into(),
                Box::new(Term::Int(1)),
                Box::new(Term::Var("y".into()))
            )
        );
    }

    #[test]
    fn effects_and_staging_forms() {
        assert_eq!(
            parse("perform log ()").unwrap(),
            Term::Perform(user("log"), Box::new(Term::Unit))
        );
        assert_eq!(
            parse("quote(1)").unwrap(),
            Term::Quote(Box::new(Term::Int(1)))
        );
        assert_eq!(
            parse("genlet(quote(1))").unwrap(),
            Term::Genlet(Box::new(Term::Quote(Box::new(Term::Int(1)))))
        );
        assert_eq!(
            parse("letloc { genlet(quote(1)) }").unwrap(),
            Term::Letloc(Box::new(Term::Genlet(Box::new(Term::Quote(Box::new(
                Term::Int(1)
            ))))))
        );
        assert_eq!(
            parse("${ c }").unwrap(),
            Term::Splice(Box::new(Term::Var("c".into())))
        );
    }

    // ── the whole point: parse → check ──────────────────────────────────

    #[test]
    fn handle_resuming_arm_sugar_parses_to_resume_call() {
        let t = parse("handle ask() with { ask(_) -> 41 }").unwrap();
        let Term::Handle(scrutinee, handler) = t else {
            panic!("expected handle")
        };
        assert_eq!(
            *scrutinee,
            Term::App(Box::new(Term::Var("ask".into())), Box::new(Term::Unit))
        );
        assert_eq!(handler.ops.len(), 1);
        let op = &handler.ops[0];
        assert_eq!(op.op, user("ask"));
        assert_eq!(op.arg, "_");
        assert_eq!(
            *op.body,
            Term::App(
                Box::new(Term::Var("resume".into())),
                Box::new(Term::Int(41))
            )
        );
        assert!(handler.ret.var.starts_with('\u{1}'));
        assert_eq!(*handler.ret.body, Term::Var(handler.ret.var.clone()));
    }

    #[test]
    fn explicit_handler_form_still_parses() {
        let src = "handle perform ask () with { ask(x) => resume x ; return(y) => y }";
        let Term::Handle(_, handler) = parse(src).unwrap() else {
            panic!("expected handle")
        };
        assert_eq!(handler.ops.len(), 1);
        assert_eq!(handler.ops[0].arg, "x");
        assert_eq!(handler.ret.var, "y");
    }

    #[test]
    fn parse_then_check_a_pure_program() {
        let t = parse("let id = fn x: Int => x in id 1").unwrap();
        let (ty, row) = infer(&Sig::new(), &Ctx::new(), 0, &t).unwrap();
        assert_eq!(ty, Type::Int);
        assert!(row.is_pure());
    }

    #[test]
    fn parse_then_check_infers_an_effect_row() {
        let t = parse("perform fs ()").unwrap();
        let (_ty, row) = infer(&Sig::new(), &Ctx::new(), 0, &t).unwrap();
        // `fs` is a native (World) effect.
        assert_eq!(row, Row::single(world("fs")));
    }

    #[test]
    fn parse_then_check_do_block_preserves_effect_rows() {
        let t = parse("effect log : Unit -> Unit in do { log(); 7 }").unwrap();
        let (ty, row) = infer(&Sig::new(), &Ctx::new(), 0, &t).unwrap();
        assert_eq!(ty, Type::Int);
        assert_eq!(row, Row::single(user("log")));
    }

    #[test]
    fn parse_then_check_checked_overflow_row_annotation() {
        let t = parse(
            "let rec f : Int -> Int ! {exn[Overflow]} = \
             fn x: Int => x +? 1 in f 1",
        )
        .unwrap();
        let (ty, row) = infer(&Sig::new(), &Ctx::new(), 0, &t).unwrap();
        assert_eq!(ty, Type::Int);
        assert_eq!(row, Row::single(Label::Exn("Overflow".into())));
    }

    #[test]
    fn parse_then_check_a_quote_does_the_delta_split() {
        let t = parse("quote(perform console ())").unwrap();
        let (ty, row) = infer(&Sig::new(), &Ctx::new(), 1, &t).unwrap();
        assert_eq!(
            ty,
            Type::Code(Box::new(Type::Unit), Row::single(world("console")))
        );
        assert!(row.is_pure());
    }

    #[test]
    fn parse_then_check_a_handler_discharges() {
        let src = "handle perform ask () with { ask(x) => resume x ; return(y) => y }";
        let t = parse(src).unwrap();
        let (_ty, row) = infer(&Sig::new(), &Ctx::new(), 0, &t).unwrap();
        assert!(row.is_pure());
    }

    // ── parse errors carry spans ────────────────────────────────────────

    #[test]
    fn trailing_input_is_an_error() {
        assert!(parse("1 2 )").is_err());
    }

    #[test]
    fn a_leading_bom_is_ignored() {
        assert_eq!(parse("\u{feff}42").unwrap(), Term::Int(42));
    }

    #[test]
    fn a_bad_token_points_at_its_column() {
        // `@` isn't a token; the error span starts at its byte offset (2).
        let e = parse("1 @ 2").unwrap_err();
        assert_eq!(e.span.map(|s| s.start), Some(2));
    }

    // ── module / program surface (sealing-plan.md S1a) ──────────────────

    #[test]
    fn a_bare_expression_is_a_module_free_program() {
        // No `module`/`import` ⇒ purely additive: same `entry` as `parse`, with
        // empty module/import lists.
        let p = parse_program("let y = 1 in y").unwrap();
        assert!(p.modules.is_empty());
        assert!(p.imports.is_empty());
        assert_eq!(p.entry, parse("let y = 1 in y").unwrap());
    }

    #[test]
    fn a_module_header_with_layer_seals_and_exposing_parses() {
        let p = parse_program(
            "module Console at services seals (winapi) exposing (console_writeln, console_write_float) = \
               let console_writeln = fn s: String => perform console s in \
               () \
             import Console \
             console_writeln \"hi\"",
        )
        .unwrap();
        assert_eq!(p.modules.len(), 1);
        let m = &p.modules[0];
        assert_eq!(m.name, "Console");
        assert_eq!(m.layer, Layer::Services);
        assert_eq!(m.seals, vec![world("winapi")]);
        assert_eq!(
            m.exposing,
            Some(vec![
                "console_writeln".to_string(),
                "console_write_float".to_string()
            ])
        );
        // The body is the let-chain ending in `()`.
        assert!(matches!(m.body, Term::Let(ref n, _, _) if n == "console_writeln"));
        assert_eq!(p.imports, vec!["Console".to_string()]);
        // The entry is what follows the last decl — not absorbed into the body.
        assert_eq!(
            p.entry,
            Term::App(
                Box::new(Term::Var("console_writeln".into())),
                Box::new(Term::Str("hi".into()))
            )
        );
    }

    #[test]
    fn the_restricted_body_grammar_stops_at_the_placeholder() {
        // Two modules back to back: the first body's `()` must terminate so the
        // second `module` is not absorbed as an application argument.
        let p = parse_program(
            "module A at boundary = () \
             module B at app = let x = 1 in () \
             0",
        )
        .unwrap();
        assert_eq!(p.modules.len(), 2);
        assert_eq!(p.modules[0].name, "A");
        assert_eq!(p.modules[0].layer, Layer::Boundary);
        assert_eq!(p.modules[0].body, Term::Unit);
        assert_eq!(p.modules[1].name, "B");
        assert!(matches!(p.modules[1].body, Term::Let(..)));
        assert_eq!(p.entry, Term::Int(0));
    }

    #[test]
    fn a_handler_wrap_module_body_parses() {
        // The seal pattern: the body is a `handle … with { … }`, ending at `}`.
        let p = parse_program(
            "module Console at services seals (winapi) exposing (console_writeln) = \
               handle (let console_writeln = fn s: String => perform console s in ()) \
               with { console(s) => resume () ; return(x) => x } \
             0",
        )
        .unwrap();
        assert_eq!(p.modules.len(), 1);
        assert!(matches!(p.modules[0].body, Term::Handle(..)));
        assert_eq!(p.entry, Term::Int(0));
    }

    #[test]
    fn a_dotted_module_name_parses() {
        let p = parse_program("module Kernel.Console at boundary = () 0").unwrap();
        assert_eq!(p.modules[0].name, "Kernel.Console");
    }

    #[test]
    fn an_unknown_layer_is_an_error() {
        let e = parse_program("module A at wizard = () 0").unwrap_err();
        assert!(e.msg.contains("unknown layer"));
    }

    #[test]
    fn a_mints_clause_parses() {
        let p = parse_program(
            "module Prov at boundary mints (crt) = \
               let s = extern \"sin\" : Float -> Float in () \
             0",
        )
        .unwrap();
        assert_eq!(p.modules[0].mints, vec![world("crt")]);
    }

    #[test]
    fn module_and_import_are_only_contextual_keywords() {
        // `module`/`import` remain ordinary identifiers inside an expression —
        // `is_stop_word` is untouched, so existing programs are unaffected.
        assert_eq!(
            parse("let module = 1 in module").unwrap(),
            Term::Let(
                "module".into(),
                Box::new(Term::Int(1)),
                Box::new(Term::Var("module".into()))
            )
        );
    }

    // ── traits / qualified types v1 (trait-resolution.md §1.1) ───────────

    #[test]
    fn a_trait_declaration_parses_to_the_decl_ast() {
        let t = parse("trait Show a { show : a -> String } in 0").unwrap();
        let Term::Trait {
            name,
            param,
            supers,
            methods,
            body,
            ..
        } = t
        else {
            panic!("expected a Term::Trait, got {t:?}");
        };
        assert_eq!(name, "Show");
        assert_eq!(param, "a");
        assert!(supers.is_empty());
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].name, "show");
        assert_eq!(
            methods[0].sig,
            Type::Fun(
                Box::new(Type::Named("a".into(), vec![])),
                Box::new(Type::Str),
                Row::pure()
            )
        );
        assert_eq!(*body, Term::Int(0));
    }

    #[test]
    fn a_trait_with_requires_and_two_methods_parses() {
        let t = parse(
            "trait Ord a requires Eq a { compare : a -> a -> Int ; lte : a -> a -> Bool } in 0",
        )
        .unwrap();
        let Term::Trait {
            name,
            supers,
            methods,
            ..
        } = t
        else {
            panic!("expected a Term::Trait");
        };
        assert_eq!(name, "Ord");
        assert_eq!(supers.len(), 1);
        assert_eq!(supers[0].trait_name, "Eq");
        assert_eq!(supers[0].ty, Type::Named("a".into(), vec![]));
        assert_eq!(methods.len(), 2);
        assert_eq!(methods[0].name, "compare");
        assert_eq!(methods[1].name, "lte");
    }

    #[test]
    fn an_instance_declaration_parses_to_the_decl_ast() {
        let t = parse("instance Show Int { show = fn x => \"n\" } in 0").unwrap();
        let Term::Instance {
            trait_name,
            head,
            requires,
            methods,
            body,
            ..
        } = t
        else {
            panic!("expected a Term::Instance, got {t:?}");
        };
        assert_eq!(trait_name, "Show");
        assert_eq!(head, Type::Int);
        assert!(requires.is_empty());
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].name, "show");
        assert!(matches!(methods[0].body, Term::Lam(..)));
        assert_eq!(*body, Term::Int(0));
    }

    #[test]
    fn an_instance_with_a_requires_context_parses() {
        let t = parse("instance Show List requires Show a { show = fn x => \"l\" } in 0").unwrap();
        let Term::Instance {
            trait_name,
            head,
            requires,
            ..
        } = t
        else {
            panic!("expected a Term::Instance");
        };
        assert_eq!(trait_name, "Show");
        assert_eq!(head, Type::Named("List".into(), vec![]));
        assert_eq!(requires.len(), 1);
        assert_eq!(requires[0].trait_name, "Show");
    }

    #[test]
    fn a_qualified_type_parses() {
        // `Show a => a -> String` parses to (one constraint, the arrow type).
        let toks = tokenize("Show a => a -> String").unwrap();
        let mut p = Parser {
            toks,
            pos: 0,
            bar_is_arm: false,
            sugar_id: 0,
            row_vars: HashMap::new(),
            current_mint: None,
        };
        let (constraints, ty) = p.qualified_ty().unwrap();
        assert_eq!(p.peek(), None, "the whole qualified type is consumed");
        assert_eq!(constraints.len(), 1);
        assert_eq!(constraints[0].trait_name, "Show");
        assert_eq!(constraints[0].ty, Type::Named("a".into(), vec![]));
        assert_eq!(
            ty,
            Type::Fun(
                Box::new(Type::Named("a".into(), vec![])),
                Box::new(Type::Str),
                Row::pure()
            )
        );
    }

    #[test]
    fn a_two_constraint_qualified_type_parses() {
        let toks = tokenize("Eq a, Show a => a -> String").unwrap();
        let mut p = Parser {
            toks,
            pos: 0,
            bar_is_arm: false,
            sugar_id: 0,
            row_vars: HashMap::new(),
            current_mint: None,
        };
        let (constraints, _ty) = p.qualified_ty().unwrap();
        assert_eq!(constraints.len(), 2);
        assert_eq!(constraints[0].trait_name, "Eq");
        assert_eq!(constraints[1].trait_name, "Show");
    }

    #[test]
    fn a_bare_type_without_constraints_still_parses_unchanged() {
        // No `=>` ⇒ the constraint-list rewind kicks in and a plain type parses.
        let toks = tokenize("List[Int] -> Bool").unwrap();
        let mut p = Parser {
            toks,
            pos: 0,
            bar_is_arm: false,
            sugar_id: 0,
            row_vars: HashMap::new(),
            current_mint: None,
        };
        let (constraints, ty) = p.qualified_ty().unwrap();
        assert!(constraints.is_empty());
        assert_eq!(
            ty,
            Type::Fun(
                Box::new(Type::Named("List".into(), vec![Type::Int])),
                Box::new(Type::Bool),
                Row::pure()
            )
        );
    }
}

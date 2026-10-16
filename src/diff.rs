//! Side-by-side diff renderer: the slice of delta (github.com/dandavison/delta,
//! MIT) equivalent to `delta --side-by-side --wrap-max-lines 0
//! --dark` — file and hunk headers, line numbers, Monokai Extended syntax
//! colours, and word-level emphasis via delta's edit inference (align.rs and
//! edits.rs, ported below). Input is a plain unified diff (`git diff`/`git show`
//! without colour); output is ratatui lines, so no external pager is needed.

use std::sync::OnceLock;

use ratatui::prelude::*;
use syntect::easy::HighlightLines;
use syntect::highlighting::Theme;
use syntect::parsing::{SyntaxReference, SyntaxSet};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use unicode_width_delta::UnicodeWidthStr as DW;

// delta's dark defaults (`delta --dark --show-config`).
const BLUE: Color = Color::Blue;
const MINUS_BG: Color = Color::Rgb(0x3f, 0x00, 0x01);
const MINUS_EMPH_BG: Color = Color::Rgb(0x90, 0x10, 0x11);
const PLUS_BG: Color = Color::Rgb(0x00, 0x28, 0x00);
const PLUS_EMPH_BG: Color = Color::Rgb(0x00, 0x60, 0x00);
const LN_MINUS: Color = Color::Indexed(88);
const LN_PLUS: Color = Color::Indexed(28);
const LN_ZERO: Color = Color::Rgb(0x44, 0x44, 0x44);
const TAB: &str = "        "; // tabs = 8
const MAX_LINE_DISTANCE: f64 = 0.6;
const LINE_BUFFER: usize = 32;

struct Assets {
    syntaxes: SyntaxSet,
    theme: Theme,
}

fn assets() -> &'static Assets {
    static ASSETS: OnceLock<Assets> = OnceLock::new();
    ASSETS.get_or_init(|| Assets {
        syntaxes: two_face::syntax::extra_newlines(),
        theme: two_face::theme::extra().get(two_face::theme::EmbeddedThemeName::MonokaiExtended).clone(),
    })
}

fn syntax_for(path: &str) -> &'static SyntaxReference {
    let ss = &assets().syntaxes;
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map_or(name, |(_, e)| e);
    ss.find_syntax_by_extension(name)
        .or_else(|| ss.find_syntax_by_extension(ext))
        .unwrap_or_else(|| ss.find_syntax_plain_text())
}

/// Syntax colours for one line: (foreground, text) runs covering `text`.
/// Lines past delta's max-syntax-highlighting-length (400) are only partly
/// highlighted; the rest keeps the default colour.
fn highlight(h: &mut HighlightLines, text: &str) -> Vec<(Color, String)> {
    const MAX: usize = 400;
    let (head, tail) = if text.len() + 1 < MAX {
        (format!("{text}\n"), "")
    } else {
        // the byte test above, but the cut is by display width (truncate_str_short)
        let (mut used, mut cut) = (0, text.len());
        for (i, c) in text.char_indices() {
            let w = c.width().unwrap_or(0);
            if used + w > MAX {
                cut = i;
                break;
            }
            used += w;
        }
        (text[..cut].to_string(), &text[cut..])
    };
    let mut runs: Vec<(Color, String)> = match h.highlight_line(&head, &assets().syntaxes) {
        Ok(runs) => runs
            .into_iter()
            .map(|(s, t)| (Color::Rgb(s.foreground.r, s.foreground.g, s.foreground.b), t.trim_end_matches('\n').to_string()))
            .filter(|(_, t)| !t.is_empty())
            .collect(),
        Err(_) => vec![(Color::Reset, head.trim_end_matches('\n').to_string())],
    };
    if !tail.is_empty() {
        runs.push((Color::Reset, tail.to_string()));
    }
    runs
}

// ---- delta's edit inference (align.rs + edits.rs) ---------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    NoOp,
    Del,
    Ins,
}

/// Tokens for alignment: `\w+` runs, every other char on its own, led by "".
fn tokenize(line: &str) -> Vec<&str> {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut matches = vec![];
    let mut start = None;
    for (i, c) in line.char_indices() {
        match (word(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                matches.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        matches.push((s, line.len()));
    }
    let chars = |s: &'_ str, base: usize, out: &mut Vec<(usize, usize)>| {
        for (i, c) in s.char_indices() {
            out.push((base + i, base + i + c.len_utf8()));
        }
    };
    let mut spans = vec![];
    let mut tokens = vec![""];
    let mut offset = 0;
    for (s, e) in matches {
        if offset == 0 && s > 0 {
            tokens.push("");
        }
        spans.clear();
        chars(&line[offset..s], offset, &mut spans);
        tokens.extend(spans.iter().map(|&(a, b)| &line[a..b]));
        tokens.push(&line[s..e]);
        offset = e;
    }
    if offset < line.len() {
        if offset == 0 {
            tokens.push("");
        }
        spans.clear();
        chars(&line[offset..], offset, &mut spans);
        tokens.extend(spans.iter().map(|&(a, b)| &line[a..b]));
    }
    tokens
}

/// Needleman-Wunsch-style alignment with delta's costs; run-length-encoded ops.
fn align(x: &[&str], y: &[&str]) -> Vec<(Op, usize)> {
    const DEL: usize = 2;
    const INS: usize = 2;
    const PENALTY: usize = 1;
    #[derive(Clone, Copy)]
    struct Cell {
        parent: usize,
        op: Op,
        cost: usize,
    }
    let w = x.len() + 1;
    let idx = |i: usize, j: usize| j * w + i;
    let mut t = vec![Cell { parent: 0, op: Op::NoOp, cost: 0 }; w * (y.len() + 1)];
    for i in 1..w {
        t[i] = Cell { parent: 0, op: Op::Del, cost: i * DEL + PENALTY };
    }
    for j in 1..=y.len() {
        t[j * w] = Cell { parent: 0, op: Op::Ins, cost: j * INS + PENALTY };
    }
    let mismatch = |t: &[Cell], p: usize, c: usize| t[p].cost + c + if t[p].op == Op::NoOp { PENALTY } else { 0 };
    for (i, xi) in x.iter().enumerate() {
        for (j, yj) in y.iter().enumerate() {
            let (left, diag, up) = (idx(i, j + 1), idx(i, j), idx(i + 1, j));
            let cands = [
                Cell { parent: up, op: Op::Ins, cost: mismatch(&t, up, INS) },
                Cell { parent: left, op: Op::Del, cost: mismatch(&t, left, DEL) },
                Cell { parent: diag, op: Op::NoOp, cost: if xi == yj { t[diag].cost } else { usize::MAX } },
            ];
            t[idx(i + 1, j + 1)] = *cands.iter().min_by_key(|c| c.cost).unwrap();
        }
    }
    let mut ops = vec![];
    let mut c = t[idx(x.len(), y.len())];
    loop {
        ops.push(c.op);
        if c.parent == 0 {
            break;
        }
        c = t[c.parent];
    }
    ops.reverse();
    let mut rle: Vec<(Op, usize)> = vec![];
    for op in ops {
        match rle.last_mut() {
            Some((o, n)) if *o == op => *n += 1,
            _ => rle.push((op, 1)),
        }
    }
    rle
}

/// (emphasised?, section) runs covering a line.
type Annotated<'a> = Vec<(bool, &'a str)>;

fn annotate<'a>(minus: &'a str, plus: &'a str) -> (Annotated<'a>, Annotated<'a>, f64) {
    let (x, y) = (tokenize(minus), tokenize(plus));
    let ops = align(&x, &y);
    let (mut am, mut ap) = (vec![], vec![]);
    let (mut xo, mut yo, mut mo, mut po) = (0, 0, 0, 0);
    let (mut numer, mut denom) = (0, 0);
    let section = |n: usize, line_off: &mut usize, tok_off: &mut usize, toks: &[&str], line: &'a str| -> &'a str {
        let len: usize = toks[*tok_off..*tok_off + n].iter().map(|s| s.len()).sum();
        let start = *line_off;
        *line_off += len;
        *tok_off += n;
        &line[start..*line_off]
    };
    let weight = |s: &str| UnicodeWidthStr::width(s.trim());
    let (mut m_prev, mut p_prev) = (false, false); // previous op was a deletion / insertion
    for (op, n) in ops {
        match op {
            Op::Del => {
                let s = section(n, &mut mo, &mut xo, &x, minus);
                numer += weight(s);
                denom += weight(s);
                am.push((true, s));
                m_prev = true;
            }
            Op::Ins => {
                let s = section(n, &mut po, &mut yo, &y, plus);
                numer += weight(s);
                denom += weight(s);
                ap.push((true, s));
                p_prev = true;
            }
            Op::NoOp => {
                let s = section(n, &mut mo, &mut xo, &x, minus);
                denom += 2 * weight(s);
                let coalesce = s.trim().is_empty()
                    && ((m_prev && p_prev && (xo < x.len() - 1 || yo < y.len() - 1)) || (!m_prev && !p_prev));
                am.push((coalesce && m_prev, s));
                let ps = section(n, &mut po, &mut yo, &y, plus);
                ap.extend(split_trailing_ws(ps).into_iter().map(|s| (coalesce && p_prev, s)));
                m_prev = false;
                p_prev = false;
            }
        }
    }
    let distance = if denom > 0 { numer as f64 / denom as f64 } else { 0.0 };
    (am, ap, distance)
}

/// Pair each minus line with its homologous plus line (greedy, in order).
/// Returns (minus index, plus index) rows plus the per-line annotations.
#[allow(clippy::type_complexity)]
fn infer_edits<'a>(minus: &[&'a str], plus: &[&'a str]) -> (Vec<Annotated<'a>>, Vec<Annotated<'a>>, Vec<(Option<usize>, Option<usize>)>) {
    let mut am: Vec<Annotated> = vec![];
    let mut ap: Vec<Annotated> = vec![];
    let mut rows = vec![];
    let mut pi = 0;
    'minus: for (mi, m) in minus.iter().enumerate() {
        for (considered, p) in plus[pi..].iter().enumerate() {
            let (a_m, a_p, d) = annotate(m, p);
            // naive pairing (equal counts) only at distance 0.0, delta's default
            if (minus.len() == plus.len() && d <= 0.0) || d <= MAX_LINE_DISTANCE {
                for k in pi..pi + considered {
                    ap.push(vec![(false, plus[k])]);
                    rows.push((None, Some(k)));
                }
                pi += considered;
                am.push(a_m);
                ap.push(a_p);
                rows.push((Some(mi), Some(pi)));
                pi += 1;
                continue 'minus;
            }
        }
        am.push(vec![(false, m)]);
        rows.push((Some(mi), None));
    }
    for k in pi..plus.len() {
        ap.push(split_trailing_ws(plus[k]).into_iter().map(|s| (false, s)).collect());
        rows.push((None, Some(k)));
    }
    (am, ap, rows)
}

/// delta's get_contents_before_trailing_whitespace: "code  " -> ["code", "  "].
fn split_trailing_ws(s: &str) -> Vec<&str> {
    let content = s.trim_end();
    if !content.is_empty() && content.len() < s.len() {
        vec![content, &s[content.len()..]]
    } else {
        vec![s]
    }
}

// ---- painting ----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Zero,
    Minus,
    Plus,
    Empty,
}

struct Cell {
    kind: Kind,
    num: Option<usize>,
    spans: Vec<Span<'static>>,
}

impl Cell {
    fn empty() -> Self {
        Cell { kind: Kind::Empty, num: None, spans: vec![] }
    }
}

/// Merge syntax runs (foreground) with edit sections (background), and mark the
/// trailing whitespace of added lines like delta's whitespace-error-style.
fn paint(text: &str, syntax: &[(Color, String)], sections: &Annotated, bg: Option<(Color, Color)>, ws_error: bool) -> Vec<Span<'static>> {
    let mut emph_at = Vec::with_capacity(text.len());
    for (e, s) in sections {
        emph_at.extend(std::iter::repeat_n(*e, s.len()));
    }
    // whitespace-error-style: trailing sections that are pure whitespace (a
    // section mixing code and trailing blanks is left alone, as in delta)
    let mut ws_from: usize = sections.iter().map(|(_, s)| s.len()).sum();
    if ws_error {
        for (_, s) in sections.iter().rev() {
            if !s.trim().is_empty() {
                break;
            }
            ws_from -= s.len();
        }
    }
    let mut spans: Vec<Span<'static>> = vec![];
    let mut pos = 0;
    for (fg, run) in syntax {
        for (i, c) in run.char_indices() {
            let at = pos + i;
            let style = if at >= ws_from {
                Style::new().fg(Color::Magenta).add_modifier(Modifier::REVERSED)
            } else {
                let mut s = Style::new().fg(*fg);
                if let Some((base, emph)) = bg {
                    s = s.bg(if emph_at.get(at).copied().unwrap_or(false) { emph } else { base });
                }
                s
            };
            match spans.last_mut() {
                Some(sp) if sp.style == style => sp.content.to_mut().push(c),
                _ => spans.push(Span::styled(c.to_string(), style)),
            }
        }
        pos += run.len();
    }
    spans
}

/// Drop terminal escape sequences and control chars from diff content (delta
/// strips them too); printed raw they would drive the terminal under the TUI.
fn sanitize(line: &str) -> String {
    if !line.chars().any(|c| c.is_control() && c != '\t') {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.next() {
                Some('[') => {
                    for c in it.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC / DCS / SOS / PM / APC: up to BEL or ST (ESC \)
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(c) = it.next() {
                        if c == '\x07' || (c == '\x1b' && it.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Cut styled spans into rows of at most `w` columns.
fn split_width(spans: Vec<Span<'static>>, w: usize) -> Vec<Vec<Span<'static>>> {
    let mut rows = vec![vec![]];
    let mut used = 0;
    for sp in spans {
        let mut cur = String::new();
        for ch in sp.content.chars() {
            let cw = ch.width().unwrap_or(0);
            if used + cw > w && used > 0 {
                if !cur.is_empty() {
                    rows.last_mut().unwrap().push(Span::styled(std::mem::take(&mut cur), sp.style));
                }
                rows.push(vec![]);
                used = 0;
            }
            used += cw;
            cur.push(ch);
        }
        if !cur.is_empty() {
            rows.last_mut().unwrap().push(Span::styled(cur, sp.style));
        }
    }
    rows
}

/// delta's `{n:^4}`: centred, the odd space going left.
fn center4(n: Option<usize>) -> String {
    let s = n.map(|n| n.to_string()).unwrap_or_default();
    let pad = 4usize.saturating_sub(s.len());
    format!("{}{s}{}", " ".repeat(pad - pad / 2), " ".repeat(pad / 2))
}

fn push_cell(out: &mut Vec<Span<'static>>, c: Cell, panel: usize, left: bool) {
    let blue = Style::new().fg(BLUE);
    let ln = match (c.kind, left) {
        (Kind::Zero, _) => LN_ZERO,
        (_, true) => LN_MINUS,
        (_, false) => LN_PLUS,
    };
    let num = center4(c.num);
    let pre = 2 + num.len(); // "│", the number column, "│"
    out.push(Span::styled("│", blue));
    out.push(Span::styled(num, Style::new().fg(ln)));
    out.push(Span::styled("│", blue));
    // delta's truncate_str over the whole panel line (number column included):
    // grapheme by grapheme, a 2-wide grapheme that doesn't fit leaves a space,
    // then the reversed "→" tail.
    let width = panel.saturating_sub(pre);
    let text: String = c.spans.iter().map(|s| s.content.as_ref()).collect();
    let mut used = DW::width(text.as_str());
    if pre + used > panel {
        // Faithful to delta, quirks included: a grapheme that doesn't fit only
        // ends its own style run (later runs may still fit one), and the space
        // filling a cut 2-wide grapheme isn't counted.
        used = pre + 1;
        for sp in c.spans {
            let mut kept = String::new();
            for g in sp.content.graphemes(true) {
                let gw = DW::width(g);
                if used + gw > panel {
                    if gw == 2 && used < panel {
                        kept.push(' ');
                    } else if gw > 2 {
                        kept.push_str(&" ".repeat(panel.saturating_sub(used)));
                    }
                    break;
                }
                kept.push_str(g);
                used += gw;
            }
            if !kept.is_empty() {
                out.push(Span::styled(kept, sp.style));
            }
        }
        out.push(Span::styled("→", Style::new().add_modifier(Modifier::REVERSED)));
        return;
    }
    out.extend(c.spans);
    let fill = " ".repeat(width.saturating_sub(used));
    match c.kind {
        Kind::Minus => out.push(Span::styled(fill, Style::new().bg(MINUS_BG))),
        Kind::Plus => out.push(Span::styled(fill, Style::new().bg(PLUS_BG))),
        _ if left => out.push(Span::raw(fill)),
        _ => {}
    }
}

/// One hunk. Like delta, a single highlighter runs through it in paint order:
/// zero lines as they come, a block's minus lines then its plus lines.
struct Hunk {
    old: usize,
    new: usize,
    hl: HighlightLines<'static>,
    minus: Vec<(String, usize)>,
    plus: Vec<(String, usize)>,
}

struct Renderer {
    width: usize,
    out: Vec<Line<'static>>,
    syntax: &'static SyntaxReference,
    hunk: Option<Hunk>,
    hunks_in_file: usize,
    // pending file header
    minus_path: Option<String>,
    plus_path: Option<String>,
    rename: (Option<String>, Option<String>),
    header_done: bool,
    /// Prefix columns per hunk line: 1, or the parent count of a merge.
    parents: usize,
    /// Gitlink (mode 160000): delta prints "old..new" instead of a hunk.
    submodule: bool,
    sub_hashes: (Option<String>, Option<String>),
    /// A gitlink's hunk header, held back: delta only draws it when the hunk
    /// turns out not to be an "old..new" update.
    pending_hunk: Option<String>,
    /// "old mode" / "new mode" -> "(mode +x)" after the file name.
    mode: (Option<String>, Option<String>),
    /// "Binary files … differ": the header says "(binary file)" instead.
    binary: bool,
}

fn strip_prefix_path(p: &str) -> String {
    let p = p.split('\t').next().unwrap_or(p);
    if p == "/dev/null" {
        return p.to_string();
    }
    p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p).to_string()
}

impl Renderer {
    fn row(&mut self, left: Cell, right: Cell) {
        let lw = self.width / 2;
        let rw = self.width - lw;
        let mut spans = vec![];
        push_cell(&mut spans, left, lw, true);
        push_cell(&mut spans, right, rw, false);
        self.out.push(Line::from(spans));
    }

    fn file_header(&mut self) {
        if self.header_done {
            return;
        }
        self.header_done = true;
        let m = self.minus_path.clone().or_else(|| self.rename.0.clone());
        let p = self.plus_path.clone().or_else(|| self.rename.1.clone());
        let text = match (m.as_deref(), p.as_deref()) {
            (Some("/dev/null"), Some(p)) => format!("added: {p}"),
            (Some(m), Some("/dev/null")) => format!("removed: {m}"),
            (Some(m), Some(p)) if m != p => format!("renamed: {m} ⟶   {p}"),
            (_, Some(p)) => p.to_string(),
            (Some(m), None) => m.to_string(),
            _ => return,
        };
        let text = match std::mem::take(&mut self.mode) {
            (Some(o), Some(n)) => {
                let m = match (o.as_str(), n.as_str()) {
                    ("100644", "100755") => "mode +x".to_string(),
                    ("100755", "100644") => "mode -x".to_string(),
                    _ => format!("mode {o} ⟶   {n}"),
                };
                format!("{text} ({m})")
            }
            _ => text,
        };
        let text = if std::mem::take(&mut self.binary) { format!("{text} (binary file)") } else { text };
        // delta picks the syntax from the new path: a removed file is plain text
        self.syntax = match p.as_deref() {
            Some("/dev/null") => assets().syntaxes.find_syntax_plain_text(),
            Some(p) => syntax_for(p),
            None => syntax_for(&m.unwrap_or_default()),
        };
        let blue = Style::new().fg(BLUE);
        self.out.push(Line::default());
        self.out.push(Line::styled(text, blue));
        self.out.push(Line::styled("─".repeat(self.width), blue));
    }

    fn flush_submodule(&mut self) {
        let (old, new) = std::mem::take(&mut self.sub_hashes);
        // only an update gets a summary; an added/removed gitlink is header-only
        let (Some(old), Some(new)) = (old, new) else { return };
        let short = |h: String| h.chars().take(12).collect::<String>();
        self.out.push(Line::from(vec![
            Span::styled(short(old), Style::new().bg(MINUS_BG)),
            Span::raw(".."),
            Span::styled(short(new), Style::new().bg(PLUS_BG)),
        ]));
    }

    fn new_hl(&self) -> HighlightLines<'static> {
        HighlightLines::new(self.syntax, &assets().theme)
    }

    fn hunk_header(&mut self, line: &str) {
        self.flush_block();
        // "@@ -a +c @@", or "@@@ -a -b +c @@@" for a merge: one column per parent
        self.parents = line.chars().take_while(|&c| c == '@').count().saturating_sub(1).max(1);
        let coords: Vec<&str> = line.trim_start_matches('@').split_whitespace().take_while(|t| !t.starts_with("@@")).collect();
        let num = |p: char| -> usize {
            coords.iter().find(|t| t.starts_with(p)).and_then(|t| t[1..].split(',').next()?.parse().ok()).unwrap_or(0)
        };
        let (old, new) = (num('-'), num('+'));
        // the code fragment after "@@ … @@", leading space included (delta's
        // HUNK_HEADER_REGEX group 2), highlighted with a trailing space and no newline
        let ctx = line.trim_start_matches('@').split_once("@@").map_or("", |(_, c)| c).trim_start_matches('@').replace('\t', TAB);

        let blue = Style::new().fg(BLUE);
        let mut mid = vec![Span::styled(new.to_string(), blue), Span::raw(":")];
        if ctx.is_empty() {
            mid.push(Span::raw(" "));
        } else {
            let mut h = self.new_hl();
            let frag = format!("{ctx} ");
            let runs = h.highlight_line(&frag, &assets().syntaxes).unwrap_or_default();
            mid.extend(runs.into_iter().map(|(st, t)| Span::styled(t.to_string(), Style::new().fg(Color::Rgb(st.foreground.r, st.foreground.g, st.foreground.b)))));
        }
        // Box around the header; a header wider than the panel wraps inside a
        // full-width box.
        let text_w: usize = mid.iter().map(|s| s.width()).sum();
        let w = text_w.min(self.width - 1);
        self.out.push(Line::default());
        self.hunks_in_file += 1;
        self.out.push(Line::from(vec![Span::styled("─".repeat(w), blue), Span::styled("┐", blue)]));
        // wrapped rows leave one blank column before the box edge, like delta
        let chunk_w = if text_w > w { w - 1 } else { w };
        if text_w > w {
            // a wrapped header's trailing blank never spills onto a row of its own
            if let Some(last) = mid.last_mut() {
                let t = last.content.trim_end().to_string();
                last.content = t.into();
            }
        }
        for mut chunk in split_width(mid, chunk_w) {
            let used: usize = chunk.iter().map(|s| s.width()).sum();
            chunk.push(Span::raw(" ".repeat(w - used)));
            chunk.push(Span::styled("│", blue));
            self.out.push(Line::from(chunk));
        }
        self.out.push(Line::from(vec![Span::styled("─".repeat(w), blue), Span::styled("┘", blue)]));
        self.hunk = Some(Hunk { old, new, hl: self.new_hl(), minus: vec![], plus: vec![] });
    }

    /// Emit the pending run of -/+ lines as paired / unpaired rows.
    fn flush_block(&mut self) {
        let Some(h) = self.hunk.as_mut() else { return };
        if h.minus.is_empty() && h.plus.is_empty() {
            return;
        }
        let minus = std::mem::take(&mut h.minus);
        let plus = std::mem::take(&mut h.plus);
        let m_runs: Vec<_> = minus.iter().map(|(t, _)| highlight(&mut h.hl, t)).collect();
        let p_runs: Vec<_> = plus.iter().map(|(t, _)| highlight(&mut h.hl, t)).collect();
        // delta aligns lines with their trailing "\n": it is a token too, which
        // decides whether a final blank gets emphasised
        let m_nl: Vec<String> = minus.iter().map(|(t, _)| format!("{t}\n")).collect();
        let p_nl: Vec<String> = plus.iter().map(|(t, _)| format!("{t}\n")).collect();
        let m_txt: Vec<&str> = m_nl.iter().map(String::as_str).collect();
        let p_txt: Vec<&str> = p_nl.iter().map(String::as_str).collect();
        let (am, ap, rows) = infer_edits(&m_txt, &p_txt);
        let mut lines = vec![];
        for (mi, pi) in rows {
            let left = mi.map_or_else(Cell::empty, |i| Cell {
                kind: Kind::Minus,
                num: Some(minus[i].1),
                spans: paint(&minus[i].0, &m_runs[i], &am[i], Some((MINUS_BG, MINUS_EMPH_BG)), false),
            });
            let right = pi.map_or_else(Cell::empty, |i| Cell {
                kind: Kind::Plus,
                num: Some(plus[i].1),
                spans: paint(&plus[i].0, &p_runs[i], &ap[i], Some((PLUS_BG, PLUS_EMPH_BG)), true),
            });
            lines.push((left, right));
        }
        for (l, r) in lines {
            self.row(l, r);
        }
    }

    fn line(&mut self, line: &str) {
        let line = &sanitize(line);
        let combined = line.strip_prefix("diff --cc ").or_else(|| line.strip_prefix("diff --combined "));
        if let Some(rest) = line.strip_prefix("diff --git ").or(combined) {
            self.flush_block();
            self.file_header(); // a previous file with no ---/+++ (rename, mode, binary)
            self.flush_submodule();
            self.submodule = false;
            self.pending_hunk = None;
            self.mode = (None, None);
            self.hunk = None;
            self.hunks_in_file = 0;
            self.header_done = false;
            self.minus_path = None;
            self.plus_path = None;
            self.parents = 1;
            let (a, b) = match (combined, rest.split_once(" b/")) {
                (None, Some((a, b))) => (strip_prefix_path(a), b.to_string()),
                _ => (rest.to_string(), rest.to_string()),
            };
            self.rename = (Some(a), Some(b));
            return;
        }
        if self.hunk.is_none() {
            if let Some(p) = line.strip_prefix("--- ") {
                self.minus_path = Some(strip_prefix_path(p));
            } else if let Some(p) = line.strip_prefix("+++ ") {
                self.plus_path = Some(strip_prefix_path(p));
                self.file_header();
            } else if let Some(p) = line.strip_prefix("rename from ") {
                self.rename.0 = Some(p.to_string());
            } else if let Some(p) = line.strip_prefix("rename to ") {
                self.rename.1 = Some(p.to_string());
            } else if let Some(m) = line.strip_prefix("old mode ") {
                self.mode.0 = Some(m.to_string());
            } else if let Some(m) = line.strip_prefix("new mode ") {
                self.mode.1 = Some(m.to_string());
            } else if line.starts_with("index ") || line.contains(" file mode ") {
                self.submodule |= line.ends_with(" 160000");
                // empty files have no ---/+++ lines; their header still says so
                if line.starts_with("new file mode") {
                    self.rename.0 = Some("/dev/null".into());
                } else if line.starts_with("deleted file mode") {
                    self.rename.1 = Some("/dev/null".into());
                }
            } else if line.starts_with("@@") && self.submodule {
                self.file_header();
                self.pending_hunk = Some(line.to_string());
                self.hunk = Some(Hunk { old: 0, new: 0, hl: self.new_hl(), minus: vec![], plus: vec![] });
            } else if line.starts_with("@@") {
                self.file_header();
                self.hunk_header(line);
            } else if line.starts_with("Binary files ") {
                self.binary = true;
            }
            // index / mode / similarity lines: folded into the header, like delta
            return;
        }
        if line.starts_with("@@") {
            self.hunk_header(line);
            return;
        }
        if self.submodule {
            if let Some(h) = line.strip_prefix("-Subproject commit ") {
                self.sub_hashes.0 = Some(h.to_string());
                return;
            }
            match line.strip_prefix("+Subproject commit ") {
                Some(h) if self.sub_hashes.0.is_some() => {
                    self.sub_hashes.1 = Some(h.to_string());
                    return;
                }
                // an added gitlink: an ordinary hunk after all
                Some(_) => {
                    if let Some(p) = self.pending_hunk.take() {
                        self.hunk_header(&p);
                    }
                }
                None => return,
            }
        }
        // delta's line-buffer-size (32): long runs are paired in batches.
        if self.hunk.as_ref().is_some_and(|h| h.minus.len() > LINE_BUFFER || h.plus.len() > LINE_BUFFER) {
            self.flush_block();
        }
        let Some(first) = line.chars().next() else { return };
        // A merge's combined diff has one prefix column per parent: any '-' makes
        // it a minus line, any '+' a plus line, all blanks a zero line — which,
        // like in delta, keeps its prefix columns.
        let (kind, text) = if self.parents > 1 {
            let n = line.char_indices().nth(self.parents).map_or(line.len(), |(i, _)| i);
            let prefix = &line[..n];
            if prefix.contains('-') {
                ('-', line[n..].replace('\t', TAB))
            } else if prefix.contains('+') {
                ('+', line[n..].replace('\t', TAB))
            } else if prefix.chars().all(|c| c == ' ') {
                (' ', line.replace('\t', TAB))
            } else {
                (first, String::new())
            }
        } else {
            (first, line[first.len_utf8()..].replace('\t', TAB))
        };
        let h = self.hunk.as_mut().unwrap();
        match kind {
            '-' => {
                if !h.plus.is_empty() {
                    self.flush_block();
                }
                let h = self.hunk.as_mut().unwrap();
                h.minus.push((text, h.old));
                h.old += 1;
            }
            '+' => {
                h.plus.push((text, h.new));
                h.new += 1;
            }
            ' ' => {
                self.flush_block();
                let h = self.hunk.as_mut().unwrap();
                let runs = highlight(&mut h.hl, &text);
                let (o, n) = (h.old, h.new);
                h.old += 1;
                h.new += 1;
                let whole: Annotated = vec![(false, text.as_str())];
                let left = Cell { kind: Kind::Zero, num: Some(o), spans: paint(&text, &runs, &whole, None, false) };
                let right = Cell { kind: Kind::Zero, num: Some(n), spans: paint(&text, &runs, &whole, None, false) };
                self.row(left, right);
            }
            _ => {
                // "\ No newline at end of file": ends the block, shown as-is
                self.flush_block();
                self.out.push(Line::raw(line.replace('\t', TAB)));
            }
        }
    }
}

/// Render a plain unified diff side by side at `width` columns.
pub fn render(diff: &str, width: u16) -> Vec<Line<'static>> {
    let mut r = Renderer {
        width: width.max(14) as usize,
        out: vec![],
        syntax: assets().syntaxes.find_syntax_plain_text(),
        hunk: None,
        hunks_in_file: 0,
        minus_path: None,
        plus_path: None,
        rename: (None, None),
        header_done: true,
        parents: 1,
        submodule: false,
        sub_hashes: (None, None),
        pending_hunk: None,
        mode: (None, None),
        binary: false,
    };
    for line in diff.lines() {
        r.line(line);
    }
    r.flush_block();
    r.file_header();
    r.flush_submodule();
    r.out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_similar_lines_and_emphasises_the_change() {
        let (am, ap, rows) = infer_edits(&["    let x = 1;"], &["    let x = 2;"]);
        assert_eq!(rows, vec![(Some(0), Some(0))]);
        let emph = |a: &Annotated| a.iter().filter(|(e, _)| *e).map(|(_, s)| *s).collect::<String>();
        assert_eq!(emph(&am[0]), "1");
        assert_eq!(emph(&ap[0]), "2");
    }

    #[test]
    fn leaves_dissimilar_lines_unpaired() {
        let (_, _, rows) = infer_edits(&["completely different line here"], &["zzz qqq www"]);
        assert_eq!(rows, vec![(Some(0), None), (None, Some(0))]);
    }

    #[test]
    fn centres_line_numbers_like_delta() {
        assert_eq!(center4(Some(1)), "  1 ");
        assert_eq!(center4(Some(61)), " 61 ");
        assert_eq!(center4(None), "    ");
    }
}

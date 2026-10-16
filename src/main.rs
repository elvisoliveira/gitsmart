//! gitsmart — read-only git dashboard for the terminal:
//!
//!   ┌───────────────┬──────────────┐
//!   │ working tree  │              │
//!   ├───────────────┤     diff     │
//!   │     log       │ side by side │
//!   ├───────┬───────┤              │
//!   │branchs│ tags  │              │
//!   └───────┴───────┴──────────────┘
//!
//! Lists filter as you type (lowercase); ⇧J/K, C-j/k and arrows move, ⇧H/L jump
//! to first/last, C-d/u half-page. Moving through a list updates the diff.
//! C-b or C-w then h/j/k/l switch panes, Tab cycles, C-b z zooms the pane, C-r
//! reloads, C-g / C-b q quit.
//! Needs only `git`: the side-by-side diff is rendered in-process (diff.rs).

mod diff;

use std::collections::{BTreeMap, HashSet};
use std::env;
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use ansi_to_tui::IntoText;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, execute, terminal};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

// Pane borders plus fzf's dark256 theme.
const ACTIVE: Color = Color::Indexed(51);
const BORDER: Color = Color::Indexed(238);
const STATUS_BG: Color = Color::Indexed(235);
const STATUS_FG: Color = Color::Indexed(245);
const SEL_BG: Color = Color::Indexed(236);
const SEL_FG: Color = Color::Indexed(254);
const POINTER: Color = Color::Indexed(161);
const PROMPT: Color = Color::Indexed(110);
const INFO: Color = Color::Indexed(144);
const HINTS: &str = " C-b hjkl: panes · type: filter · C-jk: scroll · C-du: page · C-b z: zoom · C-r: refresh · C-g/C-b q: quit ";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Worktree,
    Log,
    Branches,
    Tags,
    Diff,
}
use Pane::*;

/// Tab order.
const ORDER: [Pane; 5] = [Worktree, Log, Branches, Tags, Diff];

impl Pane {
    fn title(self) -> &'static str {
        match self {
            Worktree => "working tree",
            Log => "log",
            Branches => "branches",
            Tags => "tags",
            Diff => "diff",
        }
    }
    fn idx(self) -> usize {
        ORDER.iter().position(|&p| p == self).unwrap()
    }
}

/// What the diff pane shows.
#[derive(Clone, PartialEq, Eq)]
enum Target {
    /// Working-tree diff, or HEAD when the tree is clean.
    Seed,
    File(String),
    Commit(String),
}

struct Row {
    line: Line<'static>,
    /// Uncoloured text: filtering, hash extraction, focus-change detection.
    plain: String,
    target: Option<Target>,
}

/// One fzf-like list: "filter: <query>" on top, rows below.
struct List {
    rows: Vec<Row>,
    total: usize,
    query: String,
    cursor: usize,
    offset: usize,
    height: usize,
    /// Row under the cursor when the diff was last driven from this list —
    /// the diff only follows when it changes (fzf's `focus` event).
    focused: Option<String>,
    /// fzf --info=inline ("< n/m" beside the prompt) vs hidden.
    info: bool,
    pointer: &'static str,
    gutter: &'static str,
}

impl List {
    fn new(info: bool, pointer: &'static str, gutter: &'static str) -> Self {
        List { rows: vec![], total: 0, query: String::new(), cursor: 0, offset: 0, height: 1, focused: None, info, pointer, gutter }
    }
    fn current(&self) -> Option<&Row> {
        self.rows.get(self.cursor)
    }
    fn move_to(&mut self, i: isize) {
        let last = self.rows.len().saturating_sub(1) as isize;
        self.cursor = i.clamp(0, last) as usize;
    }
}

struct DiffView {
    target: Target,
    lines: Vec<Line<'static>>,
    scroll: usize,
    height: usize,
    /// Inner width at the last draw / at the last render request.
    width: u16,
    req_width: u16,
}

/// Everything read from git; lists are rebuilt from it on each keystroke.
#[derive(Default)]
struct Data {
    status: Vec<(String, String)>,
    graph: Vec<String>,
    flat: Vec<String>,
    branches: Vec<String>,
    tags: Vec<String>,
}

struct Req {
    gen: u64,
    target: Target,
    width: u16,
    root: String,
}

struct App {
    root: String,
    data: Data,
    lists: [List; 4],
    focus: Pane,
    zoom: bool,
    /// 'b' after C-b, 'w' after C-w: the next h/j/k/l moves between panes.
    prefix: Option<char>,
    diff: DiffView,
    rects: Vec<(Pane, Rect)>,
    req: Sender<Req>,
    gen: u64,
    quit: bool,
}

fn git(root: &str, args: &[&str]) -> String {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\x1b' {
            out.push(c);
        } else if it.next_if_eq(&'[').is_some() {
            for c in it.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            it.next();
        }
    }
    out
}

fn ansi_line(s: &str) -> Line<'static> {
    s.into_text().ok().and_then(|t| t.lines.into_iter().next()).unwrap_or_default()
}

/// First run of 7+ lowercase hex chars (capped at 40): the commit hash of a log
/// row, wherever the graph glyphs put it. Same rule as the old awk/grep.
fn hex_run(s: &str) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    let hex = |c: u8| c.is_ascii_digit() || (b'a'..=b'f').contains(&c);
    let mut i = 0;
    while i < b.len() {
        if !hex(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && hex(b[i]) {
            i += 1;
        }
        if i - start >= 7 {
            return Some((start, start + (i - start).min(40)));
        }
    }
    None
}

fn load(root: &str) -> Data {
    let status = git(root, &["-c", "core.quotepath=false", "status", "--porcelain=v1", "--untracked-files=all"])
        .lines()
        .filter(|l| l.len() > 3)
        .map(|l| {
            let path = &l[3..];
            let path = path.split_once(" -> ").map_or(path, |(_, to)| to);
            (l[..2].trim().to_string(), path.to_string())
        })
        .collect();

    // Unpushed commits get a bold-red hash.
    let unpushed: HashSet<String> =
        git(root, &["rev-list", "--abbrev-commit", "--abbrev=12", "HEAD", "--not", "--remotes"]).lines().map(String::from).collect();
    let recolor = |line: &str| match hex_run(line) {
        Some((a, b)) if unpushed.contains(&line[a..b]) => format!("{}\x1b[1;31m{}\x1b[0m{}", &line[..a], &line[a..b], &line[b..]),
        _ => line.to_string(),
    };
    let log = |graph: bool| {
        let mut args = vec!["-c", "core.abbrev=12", "-c", "color.ui=always", "log", "--color=always", "--date=format:%Y-%m-%d"];
        args.push("--pretty=format:%h %C(dim)%ad%C(reset)  %<(17,trunc)%an %C(auto)%d%C(reset)");
        args.extend(["-n", "300"]);
        if graph {
            args.push("--graph");
        }
        git(root, &args).lines().map(|l| recolor(l)).collect::<Vec<_>>()
    };

    let refs = |kind: &str| {
        git(root, &[
            "for-each-ref",
            "--sort=-creatordate",
            "--color=always",
            kind,
            "--format=%(align:20,left)%(refname:short)%(end) %(color:dim)%(objectname:short) %(committerdate:format:%Y-%m-%d)%(color:reset)",
        ])
        .lines()
        .map(String::from)
        .collect()
    };

    Data { status, graph: log(true), flat: log(false), branches: refs("refs/heads"), tags: refs("refs/tags") }
}

#[derive(Default)]
struct Node {
    kids: BTreeMap<String, Node>,
    status: String,
}

/// Changed files as a directory tree (dirs first, then files, case-insensitive),
/// keeping only paths that contain the query — so filtering still shows a tree.
fn worktree_rows(status: &[(String, String)], query: &str) -> Vec<Row> {
    let q = query.to_lowercase();
    let mut root = Node::default();
    for (st, path) in status {
        if !path.to_lowercase().contains(&q) {
            continue;
        }
        let mut n = &mut root;
        for part in path.split('/') {
            n = n.kids.entry(part.to_string()).or_default();
        }
        n.status = st.clone();
    }

    fn walk(n: &Node, prefix: &str, parts: &mut Vec<String>, rows: &mut Vec<Row>) {
        let mut keys: Vec<&String> = n.kids.keys().collect();
        keys.sort_by_key(|k| (n.kids[*k].kids.is_empty(), k.to_lowercase()));
        for (i, k) in keys.iter().enumerate() {
            let last = i + 1 == keys.len();
            let conn = if last { "└─ " } else { "├─ " };
            let child = &n.kids[*k];
            parts.push(k.to_string());
            if !child.kids.is_empty() {
                rows.push(Row {
                    line: Line::from(vec![Span::raw(format!("{prefix}{conn}")), Span::styled(format!("{k}/"), Style::new().fg(Color::Blue))]),
                    plain: format!("{}/", parts.join("/")),
                    target: None,
                });
                walk(child, &format!("{prefix}{}", if last { "   " } else { "│  " }), parts, rows);
            } else {
                let path = parts.join("/");
                rows.push(Row {
                    line: Line::from(vec![Span::raw(format!("{prefix}{conn}{k}  ")), Span::styled(child.status.clone(), Style::new().fg(Color::Yellow))]),
                    plain: path.clone(),
                    target: Some(Target::File(path)),
                });
            }
            parts.pop();
        }
    }

    let mut rows = vec![];
    walk(&root, "", &mut vec![], &mut rows);
    rows
}

/// No query: the --graph tree. With one: a flat filtered list (filtering a graph
/// would orphan its connector glyphs).
fn log_rows(d: &Data, query: &str) -> Vec<Row> {
    let q = query.to_lowercase();
    let src = if q.is_empty() { &d.graph } else { &d.flat };
    // Top entry: the uncommitted changes (worktree + index vs HEAD).
    let wt = "Working Tree/Index";
    let head = (q.is_empty() || wt.to_lowercase().contains(&q)).then(|| {
        let note = if d.status.is_empty() { " (clean)".to_string() } else { format!(" ({} changed)", d.status.len()) };
        Row {
            line: Line::from(vec![Span::styled(format!("◌ {wt}"), Style::new().fg(Color::Yellow)), Span::styled(note, Style::new().add_modifier(Modifier::DIM))]),
            plain: format!("◌ {wt}"),
            target: Some(Target::Seed),
        }
    });
    head.into_iter().chain(src.iter()
        .filter_map(|l| {
            let plain = strip_ansi(l);
            if !plain.to_lowercase().contains(&q) {
                return None;
            }
            let target = hex_run(&plain).map(|(a, b)| Target::Commit(plain[a..b].to_string()));
            Some(Row { line: ansi_line(l), plain, target })
        }))
        .collect()
}

/// fzf-style fuzzy match: every query char appears, in order (case-insensitive).
fn fuzzy(hay: &str, query: &str) -> bool {
    let mut h = hay.chars().flat_map(char::to_lowercase);
    query.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).all(|c| h.any(|x| x == c))
}

fn ref_rows(lines: &[String], query: &str) -> Vec<Row> {
    lines
        .iter()
        .filter_map(|l| {
            let plain = strip_ansi(l);
            if !fuzzy(&plain, query) {
                return None;
            }
            let name = plain.split_whitespace().next().unwrap_or("").to_string();
            let target = (!name.is_empty()).then_some(Target::Commit(name));
            Some(Row { line: ansi_line(l), plain, target })
        })
        .collect()
}

fn render_diff(root: &str, target: &Target, width: u16) -> Vec<Line<'static>> {
    let non_empty = |s: String| (!s.trim().is_empty()).then_some(s);
    // A commit: git's own coloured header (commit/Author/Date/message) as-is,
    // then its patch side by side — what delta did with `git show` output.
    let commit = |rev: &str| {
        let mut lines = git(root, &["show", "-s", "--color=always", rev]).into_text().map(|t| t.lines).unwrap_or_default();
        let patch = git(root, &["show", "--no-color", "--format=", rev]);
        if !patch.trim().is_empty() {
            lines.push(Line::default());
            lines.extend(diff::render(&patch, width));
        }
        lines
    };
    match target {
        Target::Commit(h) => commit(h),
        Target::File(p) => {
            let tracked = Command::new("git")
                .args(["-C", root, "ls-files", "--error-unmatch", "--", p])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            let raw = if tracked {
                non_empty(git(root, &["diff", "--no-color", "HEAD", "--", p])).unwrap_or_else(|| git(root, &["diff", "--no-color", "--", p]))
            } else {
                git(root, &["diff", "--no-color", "--no-index", "--", "/dev/null", p])
            };
            diff::render(&raw, width)
        }
        Target::Seed => match non_empty(git(root, &["diff", "--no-color", "HEAD"])).or_else(|| non_empty(git(root, &["diff", "--no-color"]))) {
            Some(raw) => diff::render(&raw, width),
            None => commit("HEAD"),
        },
    }
}

/// Renders diffs off the UI thread; only the newest pending request is served,
/// so fast navigation never queues up slow renders.
fn worker(rx: Receiver<Req>, tx: Sender<(u64, Vec<Line<'static>>)>) {
    while let Ok(mut r) = rx.recv() {
        while let Ok(newer) = rx.try_recv() {
            r = newer;
        }
        if tx.send((r.gen, render_diff(&r.root, &r.target, r.width))).is_err() {
            break;
        }
    }
}

fn layout(area: Rect, zoomed: Option<Pane>) -> Vec<(Pane, Rect)> {
    if let Some(p) = zoomed {
        return vec![(p, area)];
    }
    let [left, diff] = Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)]).areas(area);
    let [wt, rest] = Layout::vertical([Constraint::Percentage(34), Constraint::Percentage(66)]).areas(left);
    let [log, bottom] = Layout::vertical([Constraint::Percentage(50); 2]).areas(rest);
    let [br, tags] = Layout::horizontal([Constraint::Percentage(50); 2]).areas(bottom);
    vec![(Worktree, wt), (Log, log), (Branches, br), (Tags, tags), (Diff, diff)]
}

impl App {
    fn new(root: String, req: Sender<Req>) -> Self {
        let mut app = App {
            root,
            data: Data::default(),
            lists: [List::new(false, "▶", " "), List::new(true, "▌", "▌"), List::new(true, "▌", "▌"), List::new(true, "▌", "▌")],
            focus: Worktree,
            zoom: false,
            prefix: None,
            diff: DiffView { target: Target::Seed, lines: vec![], scroll: 0, height: 1, width: 0, req_width: 0 },
            rects: vec![],
            req,
            gen: 0,
            quit: false,
        };
        app.reset();
        app
    }

    /// Fresh load: what respawning every pane did (queries and cursors reset).
    fn reset(&mut self) {
        self.data = load(&self.root);
        for p in [Worktree, Log, Branches, Tags] {
            self.lists[p.idx()].query.clear();
            self.rebuild(p);
            let l = &mut self.lists[p.idx()];
            l.focused = l.current().map(|r| r.plain.clone());
        }
        // The diff opens on the first worktree row if it is a file, else on the
        // whole working-tree diff (HEAD when clean). Log/branches/tags don't
        // drive it until you move in them.
        self.diff.target = self.lists[0].current().and_then(|r| r.target.clone()).unwrap_or(Target::Seed);
        self.request_diff();
    }

    fn rebuild(&mut self, p: Pane) {
        let q = self.lists[p.idx()].query.clone();
        let (rows, total) = match p {
            Worktree => (worktree_rows(&self.data.status, &q), 0),
            Log => (log_rows(&self.data, &q), 0),
            Branches => (ref_rows(&self.data.branches, &q), self.data.branches.len()),
            Tags => (ref_rows(&self.data.tags, &q), self.data.tags.len()),
            Diff => return,
        };
        let l = &mut self.lists[p.idx()];
        l.total = if total == 0 { rows.len() } else { total };
        l.rows = rows;
        l.cursor = 0;
        l.offset = 0;
    }

    /// fzf `focus`: when the row under the cursor changed, point the diff at it.
    fn follow(&mut self, p: Pane) {
        let l = &mut self.lists[p.idx()];
        let cur = l.current().map(|r| r.plain.clone());
        if cur == l.focused {
            return;
        }
        l.focused = cur;
        if let Some(t) = l.current().and_then(|r| r.target.clone()) {
            if t != self.diff.target {
                self.diff.target = t;
                self.request_diff();
            }
        }
    }

    fn request_diff(&mut self) {
        if self.diff.width == 0 {
            return; // not drawn yet; the first draw requests it
        }
        self.gen += 1;
        self.diff.req_width = self.diff.width;
        let _ = self.req.send(Req { gen: self.gen, target: self.diff.target.clone(), width: self.diff.width, root: self.root.clone() });
    }

    fn set_zoom(&mut self, on: bool) {
        self.zoom = on;
    }

    fn select(&mut self, p: Pane) {
        self.set_zoom(false); // moving off a zoomed pane unzooms it
        self.focus = p;
    }

    /// Spatial pane move; wraps around at the edge.
    fn go(&mut self, dir: char) {
        if self.zoom {
            self.set_zoom(false);
            return;
        }
        let Some(&(_, c)) = self.rects.iter().find(|(p, _)| *p == self.focus) else { return };
        let overlap = |a0: u16, a1: u16, b0: u16, b1: u16| a0 < b1 && b0 < a1;
        let others = self.rects.iter().filter(|(p, _)| *p != self.focus);
        // (distance in the direction, -overlap start) — smallest wins.
        let pick = others
            .filter_map(|&(p, r)| {
                let (cand, dist) = match dir {
                    'h' => (overlap(r.y, r.bottom(), c.y, c.bottom()), (c.x as i32 - r.right() as i32).rem_euclid(1000)),
                    'l' => (overlap(r.y, r.bottom(), c.y, c.bottom()), (r.x as i32 - c.right() as i32).rem_euclid(1000)),
                    'k' => (overlap(r.x, r.right(), c.x, c.right()), (c.y as i32 - r.bottom() as i32).rem_euclid(1000)),
                    _ => (overlap(r.x, r.right(), c.x, c.right()), (r.y as i32 - c.bottom() as i32).rem_euclid(1000)),
                };
                cand.then_some((dist, r.y.max(c.y) as i32 + r.x.max(c.x) as i32, p))
            })
            .min_by_key(|&(d, o, _)| (d, o));
        if let Some((_, _, p)) = pick {
            self.focus = p;
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(prefix) = self.prefix.take() {
            match k.code {
                KeyCode::Char(c @ ('h' | 'j' | 'k' | 'l')) if !ctrl => self.go(c),
                KeyCode::Char('q') if prefix == 'b' && !ctrl => self.quit = true,
                KeyCode::Char('z') if prefix == 'b' && !ctrl => self.set_zoom(!self.zoom),
                _ => {}
            }
            return;
        }
        match k.code {
            KeyCode::Char('b') if ctrl => self.prefix = Some('b'),
            KeyCode::Char('w') if ctrl => self.prefix = Some('w'),
            KeyCode::Char('g') if ctrl => self.quit = true,
            KeyCode::Char('r') if ctrl => self.reset(),
            KeyCode::Tab => self.select(ORDER[(self.focus.idx() + 1) % ORDER.len()]),
            _ if self.focus == Diff => self.diff_key(k.code, ctrl),
            _ => self.list_key(k.code, ctrl),
        }
    }

    fn diff_key(&mut self, code: KeyCode, ctrl: bool) {
        let d = &mut self.diff;
        let half = (d.height / 2).max(1) as isize;
        let delta: isize = match code {
            KeyCode::Char('j' | 'J') | KeyCode::Down => 1,
            KeyCode::Char('k' | 'K') | KeyCode::Up => -1,
            KeyCode::Char('d') if ctrl => half,
            KeyCode::Char('u') if ctrl => -half,
            KeyCode::PageDown | KeyCode::Char(' ') => d.height as isize,
            KeyCode::PageUp => -(d.height as isize),
            KeyCode::Char('g') | KeyCode::Home => isize::MIN / 2,
            KeyCode::Char('G') | KeyCode::End => isize::MAX / 2,
            _ => return,
        };
        let max = d.lines.len().saturating_sub(d.height) as isize;
        d.scroll = (d.scroll as isize).saturating_add(delta).clamp(0, max) as usize;
    }

    fn list_key(&mut self, code: KeyCode, ctrl: bool) {
        let p = self.focus;
        let l = &mut self.lists[p.idx()];
        let half = (l.height / 2).max(1) as isize;
        let cur = l.cursor as isize;
        match code {
            KeyCode::Down | KeyCode::Char('J') => l.move_to(cur + 1),
            KeyCode::Char('j' | 'n') if ctrl => l.move_to(cur + 1),
            KeyCode::Up | KeyCode::Char('K') => l.move_to(cur - 1),
            KeyCode::Char('k' | 'p') if ctrl => l.move_to(cur - 1),
            KeyCode::Char('H') | KeyCode::Home => l.move_to(0),
            KeyCode::Char('L') | KeyCode::End => l.move_to(isize::MAX),
            KeyCode::Char('d') if ctrl => l.move_to(cur + half),
            KeyCode::Char('u') if ctrl => l.move_to(cur - half),
            KeyCode::PageDown => l.move_to(cur + l.height as isize),
            KeyCode::PageUp => l.move_to(cur - l.height as isize),
            KeyCode::Backspace => {
                if l.query.pop().is_some() {
                    self.rebuild(p);
                }
            }
            KeyCode::Char(c) if !ctrl => {
                l.query.push(c);
                self.rebuild(p);
            }
            _ => return,
        }
        self.follow(p);
    }

    fn draw(&mut self, f: &mut Frame) {
        let [main, status] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
        self.rects = layout(main, self.zoom.then_some(self.focus));
        for (p, r) in self.rects.clone() {
            let active = p == self.focus;
            let mut borders = Borders::TOP;
            if !self.zoom && matches!(p, Diff | Tags) {
                borders |= Borders::LEFT;
            }
            let style = Style::new().fg(if active { ACTIVE } else { BORDER });
            let block = Block::new().borders(borders).border_style(style).title(Span::styled(format!(" {} ", p.title()), style));
            let inner = block.inner(r);
            f.render_widget(block, r);
            match p {
                Diff => self.draw_diff(f, inner),
                _ => draw_list(f, inner, &mut self.lists[p.idx()], active),
            }
        }
        let bar = Style::new().bg(STATUS_BG).fg(STATUS_FG);
        f.render_widget(Paragraph::new(HINTS).alignment(Alignment::Right).style(bar), status);
        f.render_widget(Paragraph::new(Span::styled(" git ", bar.fg(ACTIVE).add_modifier(Modifier::BOLD))), status);
    }

    fn draw_diff(&mut self, f: &mut Frame, area: Rect) {
        let d = &mut self.diff;
        d.width = area.width;
        d.height = area.height as usize;
        d.scroll = d.scroll.min(d.lines.len().saturating_sub(d.height));
        let lines: Vec<Line> = d.lines.iter().skip(d.scroll).take(d.height).cloned().collect();
        f.render_widget(Paragraph::new(lines), area);
    }
}

fn draw_list(f: &mut Frame, area: Rect, l: &mut List, active: bool) {
    if area.height == 0 {
        return;
    }
    let dim = Style::new().fg(BORDER);
    let mut prompt = vec![Span::styled("filter: ", Style::new().fg(PROMPT)), Span::raw(l.query.clone())];
    if active {
        prompt.push(Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)));
    }
    let mut header = 1;
    if l.info {
        prompt.push(Span::styled(format!("  < {}/{} ", l.rows.len(), l.total), Style::new().fg(INFO)));
        let used: usize = prompt.iter().map(Span::width).sum();
        prompt.push(Span::styled("─".repeat((area.width as usize).saturating_sub(used)), dim));
    } else if area.height > 1 {
        header = 2;
        let sep = Rect { y: area.y + 1, height: 1, ..area };
        f.render_widget(Paragraph::new(Span::styled("─".repeat(area.width as usize), dim)), sep);
    }
    f.render_widget(Paragraph::new(Line::from(prompt)), Rect { height: 1, ..area });

    let h = area.height.saturating_sub(header) as usize;
    l.height = h.max(1);
    if l.cursor < l.offset {
        l.offset = l.cursor;
    } else if l.cursor >= l.offset + l.height {
        l.offset = l.cursor + 1 - l.height;
    }
    for (i, row) in l.rows.iter().enumerate().skip(l.offset).take(h) {
        let rect = Rect { y: area.y + header + (i - l.offset) as u16, height: 1, ..area };
        let selected = i == l.cursor;
        let mut spans = vec![if selected {
            Span::styled(l.pointer, Style::new().fg(POINTER).bg(SEL_BG))
        } else {
            Span::styled(l.gutter, Style::new().fg(SEL_BG))
        }];
        spans.push(Span::raw(" "));
        spans.extend(row.line.spans.iter().cloned());
        let style = if selected { Style::new().bg(SEL_BG).fg(SEL_FG) } else { Style::new() };
        f.render_widget(Paragraph::new(Line::from(spans)).style(style), rect);
    }
}

/// Debug-only SGR for a span style (fg, bg, reverse), to diff colours against delta.
fn sgr(st: Style) -> String {
    let c = |c: Option<Color>, base: u8| match c {
        Some(Color::Rgb(r, g, b)) => format!(";{};2;{r};{g};{b}", base + 8),
        Some(Color::Indexed(i)) => format!(";{};5;{i}", base + 8),
        Some(Color::Blue) => format!(";{}", base + 4),
        Some(Color::Magenta) => format!(";{}", base + 5),
        _ => String::new(),
    };
    let rev = if st.add_modifier.contains(Modifier::REVERSED) { ";7" } else { "" };
    format!("\x1b[0{}{}{rev}m", c(st.fg, 30), c(st.bg, 40))
}

fn restore() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
}

fn main() -> io::Result<()> {
    // Debug: `gitsmart --render-diff WIDTH < patch` prints the rendered diff as
    // plain text, to compare against `delta --side-by-side --width WIDTH`.
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("--render-diff") {
        let width = args.get(2).and_then(|w| w.parse().ok()).unwrap_or(80);
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        let ansi = args.get(3).map(String::as_str) == Some("--ansi");
        for line in diff::render(&input, width) {
            let text: String = line
                .spans
                .iter()
                .map(|s| if ansi { format!("{}{}\x1b[0m", sgr(line.style.patch(s.style)), s.content) } else { s.content.to_string() })
                .collect();
            println!("{text}");
        }
        return Ok(());
    }

    let dir = env::var("GIT_ROOT").or_else(|_| env::current_dir().map(|d| d.display().to_string())).unwrap_or_else(|_| ".".into());
    let root = git(&dir, &["rev-parse", "--show-toplevel"]).trim().to_string();
    if root.is_empty() {
        eprintln!("gitsmart: not a git repository");
        std::process::exit(1);
    }

    let (req_tx, req_rx) = mpsc::channel();
    let (res_tx, res_rx) = mpsc::channel();
    thread::spawn(move || worker(req_rx, res_tx));
    let mut app = App::new(root, req_tx);

    terminal::enable_raw_mode()?;
    execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        hook(info);
    }));
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut dirty = true;
    let result = loop {
        if dirty {
            if let Err(e) = term.draw(|f| app.draw(f)) {
                break Err(e);
            }
            if app.diff.width != app.diff.req_width {
                app.request_diff();
            }
            dirty = false;
        }
        if app.quit {
            break Ok(());
        }
        match event::poll(Duration::from_millis(30)) {
            Ok(true) => {
                match event::read() {
                    Ok(Event::Key(k)) => app.on_key(k),
                    Ok(_) => {}
                    Err(e) => break Err(e),
                }
                dirty = true;
            }
            Ok(false) => {}
            Err(e) => break Err(e),
        }
        while let Ok((gen, lines)) = res_rx.try_recv() {
            if gen == app.gen {
                app.diff.lines = lines;
                app.diff.scroll = 0;
                dirty = true;
            }
        }
    };
    restore();
    result
}

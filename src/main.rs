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
//! to first/last, C-d/u half-page. Moving through a list updates the diff;
//! moving through branches also points the log at that branch.
//! C-b or C-w then h/j/k/l switch panes, Tab switches the pane's tab (branches:
//! local/remote, tags: tags/stashes, diff: diff/properties), C-b z zooms the
//! pane, w (in the diff) toggles wrapping long lines, C-r reloads, C-g / C-b q quit.
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
const HINTS: &str = " C-b hjkl: panes · Tab: tab · type: filter · Esc: clear · C-jk: scroll · C-du: page · C-b z: zoom · w: wrap · C-r: refresh · C-g/C-b q: quit ";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Worktree,
    Log,
    Branches,
    Tags,
    Diff,
}
use Pane::*;

const ORDER: [Pane; 5] = [Worktree, Log, Branches, Tags, Diff];
/// `lists` slots of the second tabs (the first tab of a pane uses `p.idx()`).
const REMOTES: usize = 4;
const STASHES: usize = 5;

impl Pane {
    /// Tab names; Tab cycles them. The first one is the pane's title.
    fn tabs(self) -> &'static [&'static str] {
        match self {
            Worktree => &["working tree"],
            Log => &["log"],
            Branches => &["local", "remote"],
            Tags => &["tags", "stashes"],
            Diff => &["diff", "properties"],
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
    /// `stash@{n}`: a merge commit, so its patch comes from `git stash show`.
    Stash(String),
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
    /// Scroll of the properties tab, kept apart from the diff's.
    props_scroll: usize,
    height: usize,
    /// Inner width at the last draw / at the last render request.
    width: u16,
    req_width: u16,
    /// Long lines wrap onto extra rows instead of ending in "→" (toggled with `w`).
    wrap: bool,
}

/// Everything read from git; lists are rebuilt from it on each keystroke.
#[derive(Default)]
struct Data {
    status: Vec<(String, String)>,
    log: Log,
    branches: Vec<String>,
    remotes: Vec<String>,
    tags: Vec<String>,
    stashes: Vec<String>,
    /// The properties tab: remotes and local config.
    props: Vec<Line<'static>>,
    /// Checked-out branch ("" when detached).
    current: String,
}

/// The log of one rev, in the two shapes the pane shows.
#[derive(Default)]
struct Log {
    graph: Vec<String>,
    flat: Vec<String>,
    /// Full hash of each `flat` row, so the log filter matches any hash prefix.
    full: Vec<String>,
}

struct Req {
    gen: u64,
    target: Target,
    width: u16,
    wrap: bool,
    root: String,
}

struct LogReq {
    gen: u64,
    root: String,
    rev: String,
}

struct App {
    root: String,
    data: Data,
    /// Worktree, log, branches, tags, remote branches, stashes.
    lists: [List; 6],
    focus: Pane,
    /// Active tab of each pane, by `idx()`.
    tab: [usize; 5],
    zoom: bool,
    /// 'b' after C-b, 'w' after C-w: the next h/j/k/l moves between panes.
    prefix: Option<char>,
    diff: DiffView,
    rects: Vec<(Pane, Rect)>,
    req: Sender<Req>,
    gen: u64,
    /// Full ref (`refs/heads/x`, `refs/remotes/o/x`) whose history the log
    /// shows; None = HEAD. Follows the branches pane.
    log_ref: Option<String>,
    log_req: Sender<LogReq>,
    log_gen: u64,
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

const LOG_FMT: &str = "%h %C(dim)%ad%C(reset)  %<(17,trunc)%an %C(auto)%d%C(reset)";

/// One `git log --graph` read gives every shape the pane needs: each commit line
/// carries its full hash before a \x1f, cut out of the graph and flat rows.
fn load_log(root: &str, rev: &str) -> Log {
    // Unpushed commits get a bold-red hash.
    let unpushed: HashSet<String> =
        git(root, &["rev-list", "--abbrev-commit", "--abbrev=12", rev, "--not", "--remotes"]).lines().map(String::from).collect();
    let fmt = format!("--pretty=format:%H%x1f{LOG_FMT}");
    let raw = git(root, &["-c", "core.abbrev=12", "-c", "color.ui=always", "log", "--graph", "--color=always", "--date=format:%Y-%m-%d", &fmt, "-n", "300", rev, "--"]);
    parse_log(&raw, &unpushed)
}

fn parse_log(raw: &str, unpushed: &HashSet<String>) -> Log {
    let recolor = |line: &str| match hex_run(line) {
        Some((a, b)) if unpushed.contains(&line[a..b]) => format!("{}\x1b[1;31m{}\x1b[0m{}", &line[..a], &line[a..b], &line[b..]),
        _ => line.to_string(),
    };
    let mut log = Log::default();
    for line in raw.lines() {
        match line.split_once('\x1f') {
            Some((head, rest)) if head.len() >= 40 => {
                let (glyphs, full) = head.split_at(head.len() - 40);
                log.graph.push(recolor(&format!("{glyphs}{rest}")));
                log.flat.push(recolor(rest));
                log.full.push(full.to_string());
            }
            _ => log.graph.push(line.to_string()), // connector-only graph line
        }
    }
    log
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

    let refs = |kind: &str| {
        git(root, &[
            "for-each-ref",
            "--sort=-creatordate",
            "--color=always",
            kind,
            "--format=%(refname:short)",
        ])
        .lines()
        .map(String::from)
        .collect()
    };

    // Symrefs (origin/HEAD, short-named just "origin") come out as empty lines.
    let remotes = git(root, &["for-each-ref", "--sort=-creatordate", "--format=%(if)%(symref)%(then)%(else)%(refname:short)%(end)", "refs/remotes"])
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    let stashes = git(root, &["stash", "list", "--format=%gd %s"]).lines().map(String::from).collect();
    let current = git(root, &["branch", "--show-current"]).trim().to_string();
    Data {
        status,
        log: load_log(root, "HEAD"),
        branches: refs("refs/heads"),
        remotes,
        tags: refs("refs/tags"),
        stashes,
        props: props_lines(root),
        current,
    }
}

/// `git remote -v` and `git config --local --list`, credentials masked.
fn props_lines(root: &str) -> Vec<Line<'static>> {
    let head = |s: &str| Line::styled(s.to_string(), Style::new().fg(PROMPT).add_modifier(Modifier::BOLD));
    let mut lines = vec![head("remotes")];
    let remotes = git(root, &["remote", "-v"]);
    // Tabs would break the cell grid.
    lines.extend(remotes.lines().map(|l| Line::raw(format!("  {}", mask_url(l).replace('\t', "  ")))));
    if remotes.trim().is_empty() {
        lines.push(Line::raw("  (none)"));
    }
    lines.push(Line::default());
    lines.push(head("config (local)"));
    for l in git(root, &["config", "--local", "--list"]).lines() {
        let (k, v) = l.split_once('=').unwrap_or((l, ""));
        lines.push(Line::from(vec![Span::raw("  "), Span::styled(k.to_string(), Style::new().fg(INFO)), Span::raw(format!(" = {}", mask(k, v)))]));
    }
    lines
}

/// Config values that are secrets as a whole (tokens, auth headers).
fn mask(key: &str, value: &str) -> String {
    let k = key.to_lowercase();
    if ["extraheader", "token", "password", "secret"].iter().any(|w| k.contains(w)) {
        return "***".into();
    }
    mask_url(value)
}

/// `https://user:token@host/...` → `https://***@host/...`. ssh's `user@` (no
/// password) stays.
fn mask_url(s: &str) -> String {
    let Some(i) = s.find("://") else { return s.to_string() };
    let rest = &s[i + 3..];
    let end = rest.find(['/', ' ', '\t']).unwrap_or(rest.len());
    match rest[..end].rfind('@') {
        Some(at) if rest[..at].contains(':') || s[..i].starts_with("http") => format!("{}://***{}", &s[..i], &rest[at..]),
        _ => s.to_string(),
    }
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
/// would orphan its connector glyphs). `on_head`: the log shows HEAD, so the
/// uncommitted-changes row belongs on top.
fn log_rows(d: &Data, query: &str, on_head: bool) -> Vec<Row> {
    let q = query.to_lowercase();
    let src = if q.is_empty() { &d.log.graph } else { &d.log.flat };
    // Top entry: the uncommitted changes (worktree + index vs HEAD).
    let wt = "Working Tree/Index";
    let head = (on_head && (q.is_empty() || wt.to_lowercase().contains(&q))).then(|| {
        let note = if d.status.is_empty() { " (clean)".to_string() } else { format!(" ({} changed)", d.status.len()) };
        Row {
            line: Line::from(vec![Span::styled(format!("◌ {wt}"), Style::new().fg(Color::Yellow)), Span::styled(note, Style::new().add_modifier(Modifier::DIM))]),
            plain: format!("◌ {wt}"),
            target: Some(Target::Seed),
        }
    });
    // Filtering uses `flat`, row-aligned with `full`; the graph is only shown unfiltered.
    head.into_iter().chain(src.iter().enumerate()
        .filter_map(|(i, l)| {
            let plain = strip_ansi(l);
            let by_hash = is_hex(&q) && d.log.full.get(i).is_some_and(|h| h.starts_with(&q));
            if !by_hash && !plain.to_lowercase().contains(&q) {
                return None;
            }
            let target = hex_run(&plain).map(|(a, b)| Target::Commit(plain[a..b].to_string()));
            Some(Row { line: ansi_line(l), plain, target })
        }))
        .collect()
}

fn is_hex(q: &str) -> bool {
    q.len() >= 4 && q.bytes().all(|c| c.is_ascii_hexdigit())
}

/// fzf-style fuzzy match: every query char appears, in order (case-insensitive).
fn fuzzy(hay: &str, query: &str) -> bool {
    let mut h = hay.chars().flat_map(char::to_lowercase);
    query.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).all(|c| h.any(|x| x == c))
}

/// `current`: the checked-out branch, marked `*` ("" for tags: no marks).
fn ref_rows(lines: &[String], query: &str, current: &str) -> Vec<Row> {
    lines
        .iter()
        .filter_map(|l| {
            let plain = strip_ansi(l);
            if !fuzzy(&plain, query) {
                return None;
            }
            let name = plain.split_whitespace().next().unwrap_or("").to_string();
            let mut line = ansi_line(l);
            if !current.is_empty() {
                let mark = if name == current { Span::styled("* ", Style::new().fg(Color::Green)) } else { Span::raw("  ") };
                line.spans.insert(0, mark);
            }
            let target = (!name.is_empty()).then_some(Target::Commit(name));
            Some(Row { line, plain, target })
        })
        .collect()
}

/// Everything in the commit object (`git show --format=raw`: tree, parents,
/// author, committer, signature, ...) plus readable dates and the refs, then
/// the message.
fn commit_header(root: &str, rev: &str) -> Vec<Line<'static>> {
    let info = git(root, &["show", "-s", "--date=format:%Y-%m-%d %H:%M %z", "--format=%ad%x00%cd%x00%d", rev]);
    let mut info = info.trim_end_matches('\n').split('\0');
    let mut next = || info.next().unwrap_or("").to_string();
    let (ad, cd, refs) = (next(), next(), next());
    header_lines(&git(root, &["show", "-s", "--format=raw", rev]), &ad, &cd, &refs)
}

/// Multi-line headers (gpgsig, mergetag) fold to one line; the signature is
/// only named, never verified (that needs gpg and its keys).
fn header_lines(raw: &str, ad: &str, cd: &str, refs: &str) -> Vec<Line<'static>> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let yellow = Style::new().fg(Color::Yellow);
    let mut lines = vec![];
    let mut it = raw.lines().peekable();
    while let Some(l) = it.next() {
        if l.is_empty() {
            lines.push(Line::default());
            lines.extend(it.map(|m| Line::raw(m.to_string())));
            break;
        }
        let (key, val) = l.split_once(' ').unwrap_or((l, ""));
        let mut more = 0;
        while it.next_if(|n| n.starts_with(' ')).is_some() {
            more += 1;
        }
        let mut spans = vec![Span::styled(format!("{key:<9} "), Style::new().fg(Color::Blue))];
        match key {
            "commit" => spans.extend([Span::styled(val.to_string(), yellow), Span::styled(refs.to_string(), yellow)]),
            "author" | "committer" => {
                let date = if key == "author" { ad } else { cd };
                spans.extend([Span::raw(val.to_string()), Span::styled(format!("  ({date})"), dim)]);
            }
            k if k.starts_with("gpgsig") => {
                let kind = val.trim_start_matches("-----BEGIN ").trim_end_matches("-----");
                spans.push(Span::raw(format!("{kind}, {} lines, not verified", more + 1)));
            }
            _ if more > 0 => spans.push(Span::raw(format!("{val} (+{more} lines)"))),
            _ => spans.push(Span::raw(val.to_string())),
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn render_diff(root: &str, target: &Target, width: u16, wrap: bool) -> Vec<Line<'static>> {
    let non_empty = |s: String| (!s.trim().is_empty()).then_some(s);
    // A commit: git's own coloured header (commit/Author/Date/message) as-is,
    // then its patch side by side — what delta did with `git show` output.
    let commit = |rev: &str| {
        let mut lines = commit_header(root, rev);
        let patch = git(root, &["show", "--no-color", "--format=", rev]);
        if !patch.trim().is_empty() {
            lines.push(Line::default());
            lines.extend(diff::render(&patch, width, wrap));
        }
        lines
    };
    match target {
        Target::Commit(h) => commit(h),
        Target::Stash(s) => {
            let mut lines = commit_header(root, s);
            let patch = git(root, &["stash", "show", "-p", "--include-untracked", "--no-color", s]);
            if !patch.trim().is_empty() {
                lines.push(Line::default());
                lines.extend(diff::render(&patch, width, wrap));
            }
            lines
        }
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
            diff::render(&raw, width, wrap)
        }
        Target::Seed => match non_empty(git(root, &["diff", "--no-color", "HEAD"])).or_else(|| non_empty(git(root, &["diff", "--no-color"]))) {
            Some(raw) => diff::render(&raw, width, wrap),
            None => commit("HEAD"),
        },
    }
}

/// Runs `f` off the UI thread; only the newest pending request is served, so
/// fast navigation never queues up slow work (diff renders, log loads).
fn spawn_latest<T: Send + 'static, R: Send + 'static>(f: impl Fn(T) -> R + Send + 'static) -> (Sender<T>, Receiver<R>) {
    let (tx, rx) = mpsc::channel::<T>();
    let (res_tx, res_rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(mut r) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                r = newer;
            }
            if res_tx.send(f(r)).is_err() {
                break;
            }
        }
    });
    (tx, res_rx)
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
    fn new(root: String, req: Sender<Req>, log_req: Sender<LogReq>) -> Self {
        let mut app = App {
            root,
            data: Data::default(),
            lists: [List::new(false, "▶", " "), List::new(true, "▌", "▌"), List::new(true, "▌", "▌"), List::new(true, "▌", "▌"), List::new(true, "▌", "▌"), List::new(true, "▌", "▌")],
            focus: Worktree,
            tab: [0; 5],
            zoom: false,
            prefix: None,
            diff: DiffView { target: Target::Seed, lines: vec![], scroll: 0, props_scroll: 0, height: 1, width: 0, req_width: 0, wrap: false },
            rects: vec![],
            req,
            gen: 0,
            log_ref: None,
            log_req,
            log_gen: 0,
            quit: false,
        };
        app.reset();
        app
    }

    /// Fresh load: what respawning every pane did (queries and cursors reset).
    fn reset(&mut self) {
        self.data = load(&self.root);
        self.log_ref = None;
        self.log_gen += 1; // drop a branch log still loading
        let tabs = self.tab;
        for p in [Worktree, Log, Branches, Tags] {
            for t in 0..p.tabs().len() {
                self.tab[p.idx()] = t;
                let i = self.li(p);
                self.lists[i].query.clear();
                self.rebuild(p);
                let l = &mut self.lists[i];
                l.focused = l.current().map(|r| r.plain.clone());
            }
        }
        self.tab = tabs;
        let current = format!("refs/heads/{}", self.data.current);
        self.point_branches_at(&current);
        // The diff opens on the first worktree row if it is a file, else on the
        // whole working-tree diff (HEAD when clean). Log/branches/tags don't
        // drive it until you move in them.
        self.diff.target = self.lists[0].current().and_then(|r| r.target.clone()).unwrap_or(Target::Seed);
        self.request_diff();
    }

    /// `lists` slot of the pane's active tab.
    fn li(&self, p: Pane) -> usize {
        match (p, self.tab[p.idx()]) {
            (Branches, 1) => REMOTES,
            (Tags, 1) => STASHES,
            _ => p.idx(),
        }
    }

    fn rebuild(&mut self, p: Pane) {
        let i = self.li(p);
        let q = self.lists[i].query.clone();
        let (rows, total) = match p {
            Worktree => (worktree_rows(&self.data.status, &q), 0),
            Log => {
                let mut rows = log_rows(&self.data, &q, self.log_ref.is_none());
                let found = |rows: &[Row]| rows.iter().any(|r| matches!(r.target, Some(Target::Commit(_))));
                // A hash not in the loaded log: show the local branch holding it, so
                // the row is a real log row (and Esc keeps it). Else ask git for
                // that one commit (remote-only, or past the 300 loaded).
                if q.len() >= 7 && is_hex(&q) && !found(&rows) {
                    let rev = format!("{}^{{commit}}", q.to_lowercase());
                    if let Some(b) = self.branch_containing(&rev) {
                        self.point_branches_at(&b);
                        self.show_log_of(&b, true);
                        rows = log_rows(&self.data, &q, self.log_ref.is_none());
                    }
                }
                if q.len() >= 7 && is_hex(&q) && !found(&rows) {
                    let rev = format!("{}^{{commit}}", q.to_lowercase());
                    let fmt = format!("--pretty=format:{LOG_FMT}");
                    let l = git(&self.root, &["-c", "core.abbrev=12", "-c", "color.ui=always", "log", "-1", "--color=always", "--date=format:%Y-%m-%d", &fmt, &rev, "--"]);
                    if let Some(l) = l.lines().next() {
                        let plain = strip_ansi(l);
                        let target = hex_run(&plain).map(|(a, b)| Target::Commit(plain[a..b].to_string()));
                        rows.push(Row { line: ansi_line(l), plain, target });
                    }
                }
                (rows, 0)
            }
            Branches if i == REMOTES => (ref_rows(&self.data.remotes, &q, ""), self.data.remotes.len()),
            Branches => (ref_rows(&self.data.branches, &q, &self.data.current), self.data.branches.len()),
            Tags if i == STASHES => {
                let mut rows = ref_rows(&self.data.stashes, &q, "");
                for r in &mut rows {
                    if let Some(Target::Commit(s)) = r.target.take() {
                        r.target = Some(Target::Stash(s));
                    }
                }
                (rows, self.data.stashes.len())
            }
            Tags => (ref_rows(&self.data.tags, &q, ""), self.data.tags.len()),
            Diff => return,
        };
        let l = &mut self.lists[i];
        l.total = if total == 0 { rows.len() } else { total };
        l.rows = rows;
        l.cursor = 0;
        l.offset = 0;
    }

    /// fzf `focus`: when the row under the cursor changed, point the diff at it.
    fn follow(&mut self, p: Pane) {
        let i = self.li(p);
        let l = &mut self.lists[i];
        let cur = l.current().map(|r| r.plain.clone());
        if cur == l.focused {
            return;
        }
        l.focused = cur.clone();
        if let Some(t) = l.current().and_then(|r| r.target.clone()) {
            if t != self.diff.target {
                self.diff.target = t;
                self.request_diff();
            }
        }
        if let (Branches, Some(b)) = (p, cur) {
            let kind = if i == REMOTES { "remotes" } else { "heads" };
            self.show_log_of(&format!("refs/{kind}/{b}"), false);
        }
    }

    /// Point the log at a branch (full ref): the checked-out one means HEAD
    /// (with the working-tree row). Async while navigating branches; sync for a
    /// hash search, which needs the rows right away.
    fn show_log_of(&mut self, full: &str, now: bool) {
        let r = (full != format!("refs/heads/{}", self.data.current)).then(|| full.to_string());
        if r == self.log_ref {
            return;
        }
        self.log_ref = r;
        self.log_gen += 1;
        let rev = self.log_ref.clone().unwrap_or_else(|| "HEAD".into());
        if now {
            self.data.log = load_log(&self.root, &rev);
        } else {
            let _ = self.log_req.send(LogReq { gen: self.log_gen, root: self.root.clone(), rev });
        }
    }

    /// A branch log finished loading in the background.
    fn set_log(&mut self, gen: u64, log: Log) {
        if gen != self.log_gen {
            return;
        }
        self.data.log = log;
        self.rebuild(Log);
        let l = &mut self.lists[Log.idx()];
        l.focused = l.current().map(|r| r.plain.clone());
    }

    /// Branch holding a commit, as a full ref: the checked-out one if it does,
    /// else the most recent local one (the branches pane's order), else the
    /// most recent remote one.
    fn branch_containing(&self, rev: &str) -> Option<String> {
        let has: HashSet<String> = git(&self.root, &["for-each-ref", "--contains", rev, "--format=%(refname)", "refs/heads", "refs/remotes"]).lines().map(String::from).collect();
        let d = &self.data;
        let local = |b: &str| format!("refs/heads/{b}");
        if has.contains(&local(&d.current)) {
            return Some(local(&d.current));
        }
        let remotes = d.remotes.iter().map(|b| format!("refs/remotes/{b}"));
        d.branches.iter().map(|l| local(&strip_ansi(l))).chain(remotes).find(|b| has.contains(b))
    }

    /// Put the branches cursor on a branch (full ref), on its tab, without
    /// moving the diff (no-op when it is filtered out).
    fn point_branches_at(&mut self, full: &str) {
        let (tab, slot, name) = match full.strip_prefix("refs/remotes/") {
            Some(n) => (1, REMOTES, n),
            None => (0, Branches.idx(), full.strip_prefix("refs/heads/").unwrap_or(full)),
        };
        let l = &mut self.lists[slot];
        if let Some(i) = l.rows.iter().position(|r| r.plain == name) {
            l.cursor = i;
            l.focused = Some(name.to_string());
            self.tab[Branches.idx()] = tab;
        }
    }

    /// Tab: the focused pane's next tab. The diff follows the row under the
    /// cursor there (and the log, on the branches tabs).
    fn next_tab(&mut self) {
        let p = self.focus;
        let n = p.tabs().len();
        self.tab[p.idx()] = (self.tab[p.idx()] + 1) % n;
        if n > 1 && p != Diff {
            let i = self.li(p);
            self.lists[i].focused = None;
            self.follow(p);
        }
    }

    fn request_diff(&mut self) {
        if self.diff.width == 0 {
            return; // not drawn yet; the first draw requests it
        }
        self.gen += 1;
        self.diff.req_width = self.diff.width;
        let _ = self.req.send(Req { gen: self.gen, target: self.diff.target.clone(), width: self.diff.width, wrap: self.diff.wrap, root: self.root.clone() });
    }

    fn set_zoom(&mut self, on: bool) {
        self.zoom = on;
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
            KeyCode::Tab => self.next_tab(),
            _ if self.focus == Diff => self.diff_key(k.code, ctrl),
            _ => self.list_key(k.code, ctrl),
        }
    }

    fn diff_key(&mut self, code: KeyCode, ctrl: bool) {
        let props = self.tab[Diff.idx()] == 1;
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
            KeyCode::Char('w') if !ctrl => {
                d.wrap = !d.wrap;
                self.request_diff();
                return;
            }
            _ => return,
        };
        let (len, scroll) = if props { (self.data.props.len(), &mut d.props_scroll) } else { (d.lines.len(), &mut d.scroll) };
        let max = len.saturating_sub(d.height) as isize;
        *scroll = (*scroll as isize).saturating_add(delta).clamp(0, max) as usize;
    }

    fn list_key(&mut self, code: KeyCode, ctrl: bool) {
        let p = self.focus;
        let i = self.li(p);
        let l = &mut self.lists[i];
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
            // Clear the filter but stay on the same row. Matched by target, since a
            // log row's text differs between the flat (filtered) and graph views.
            KeyCode::Esc if !l.query.is_empty() => {
                let (query, cursor) = (std::mem::take(&mut l.query), l.cursor);
                let sel = l.current().map(|r| (r.target.clone(), r.plain.clone()));
                self.rebuild(p);
                let l = &mut self.lists[i];
                let found = sel.map(|(target, plain)| l.rows.iter().position(|r| if target.is_some() { r.target == target } else { r.plain == plain }));
                match found {
                    Some(Some(i)) => l.cursor = i,
                    // Not in the unfiltered list (remote-only, or past the 300
                    // loaded): keep the filter rather than lose the row.
                    Some(None) => {
                        l.query = query;
                        self.rebuild(p);
                        self.lists[i].cursor = cursor;
                    }
                    None => {}
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
            let title = match (p, &self.log_ref) {
                (Log, Some(b)) => {
                    let b = b.strip_prefix("refs/heads/").or_else(|| b.strip_prefix("refs/remotes/")).unwrap_or(b);
                    Line::styled(format!(" log · {b} "), style)
                }
                // Tabs: the active one bold and underlined.
                _ => {
                    let mut spans = vec![Span::styled(" ", style)];
                    for (t, name) in p.tabs().iter().enumerate() {
                        if t > 0 {
                            spans.push(Span::styled(" │ ", style));
                        }
                        let on = t == self.tab[p.idx()] && p.tabs().len() > 1;
                        spans.push(Span::styled(*name, if on { style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED) } else { style }));
                    }
                    spans.push(Span::styled(" ", style));
                    Line::from(spans)
                }
            };
            let block = Block::new().borders(borders).border_style(style).title(title);
            let inner = block.inner(r);
            f.render_widget(block, r);
            match p {
                Diff => self.draw_diff(f, inner),
                _ => {
                    let i = self.li(p);
                    draw_list(f, inner, &mut self.lists[i], active)
                }
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
        let (src, scroll) = if self.tab[Diff.idx()] == 1 { (&self.data.props, &mut d.props_scroll) } else { (&d.lines, &mut d.scroll) };
        *scroll = (*scroll).min(src.len().saturating_sub(d.height));
        let lines: Vec<Line> = src.iter().skip(*scroll).take(d.height).cloned().collect();
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
        for line in diff::render(&input, width, false) {
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

    let (req_tx, res_rx) = spawn_latest(|r: Req| (r.gen, render_diff(&r.root, &r.target, r.width, r.wrap)));
    let (log_tx, log_rx) = spawn_latest(|r: LogReq| (r.gen, load_log(&r.root, &r.rev)));
    let mut app = App::new(root, req_tx, log_tx);

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
        while let Ok((gen, log)) = log_rx.try_recv() {
            app.set_log(gen, log);
            dirty = true;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn app(root: &str) -> App {
        let (tx, _) = mpsc::channel();
        let (log_tx, _) = mpsc::channel();
        App::new(root.into(), tx, log_tx)
    }

    fn key(app: &mut App, c: KeyCode) {
        app.on_key(KeyEvent::new(c, KeyModifiers::NONE));
    }

    fn typed(app: &mut App, s: &str) {
        s.chars().for_each(|c| key(app, KeyCode::Char(c)));
    }

    fn selected(app: &App, p: Pane) -> Option<Target> {
        app.lists[app.li(p)].current().and_then(|r| r.target.clone())
    }

    #[test]
    fn log_filter_matches_full_hash_prefix() {
        let full = "0123456789abcdef0123456789abcdef01234567";
        let log = Log { flat: vec!["0123456789ab 2026-01-01  me".into()], full: vec![full.into()], ..Default::default() };
        let d = Data { log, ..Default::default() };
        let hit = |q: &str| log_rows(&d, q, true).iter().any(|r| r.target == Some(Target::Commit("0123456789ab".into())));
        assert!(hit("0123456789ab"));    // short hash, as shown
        assert!(hit(full));              // pasted full hash
        assert!(hit("0123456789abcdef")); // longer than the shown abbrev
        assert!(!hit("0123456789abcdee"));
    }

    #[test]
    fn parse_log_splits_graph_flat_and_full_hash() {
        let (a, b) = ("a".repeat(40), "b".repeat(40));
        let raw = format!("* {a}\x1f{} first\n|\\\n| * {b}\x1f{} second", &a[..12], &b[..12]);
        let log = parse_log(&raw, &HashSet::new());
        assert_eq!(log.graph, [format!("* {} first", &a[..12]), "|\\".into(), format!("| * {} second", &b[..12])]);
        assert_eq!(log.flat, [format!("{} first", &a[..12]), format!("{} second", &b[..12])]);
        assert_eq!(log.full, [a, b]);
    }

    #[test]
    fn esc_clears_filter_and_keeps_the_row() {
        let mut app = app("/nonexistent");
        let (a, b) = ("aaaaaaaaaaaa", "bbbbbbbbbbbb");
        app.data.log = Log {
            graph: vec![format!("* {a} first"), format!("* {b} second")],
            flat: vec![format!("{a} first"), format!("{b} second")],
            full: vec![a.repeat(4)[..40].into(), b.repeat(4)[..40].into()],
        };
        app.focus = Log;
        typed(&mut app, "second");
        key(&mut app, KeyCode::Esc);
        assert!(app.lists[Log.idx()].query.is_empty());
        assert!(selected(&app, Log) == Some(Target::Commit(b.into())));
    }

    #[test]
    fn header_shows_every_field_and_folds_the_signature() {
        let raw = "commit abc\ntree t1\nparent p1\nparent p2\nauthor A <a@a> 1791364891 +0100\ncommitter C <c@c> 1791364900 +0100\n\
                   gpgsig -----BEGIN PGP SIGNATURE-----\n \n xyz\n -----END PGP SIGNATURE-----\n\n    subject\n\n    body";
        let text: Vec<String> = header_lines(raw, "2026-10-07 10:21 +0100", "2026-10-07 10:22 +0100", " (HEAD -> main)")
            .iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect();
        assert_eq!(text, [
            "commit    abc (HEAD -> main)",
            "tree      t1",
            "parent    p1",
            "parent    p2",
            "author    A <a@a> 1791364891 +0100  (2026-10-07 10:21 +0100)",
            "committer C <c@c> 1791364900 +0100  (2026-10-07 10:22 +0100)",
            "gpgsig    PGP SIGNATURE, 4 lines, not verified",
            "",
            "    subject",
            "",
            "    body",
        ]);
    }

    /// The reported case: a hash that only lives on another local branch.
    #[test]
    fn hash_on_another_branch_moves_branches_and_survives_esc() {
        let dir = std::env::temp_dir().join(format!("gitsmart-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.to_str().unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git").arg("-C").arg(root).args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"]).args(args)
                .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap().success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["commit", "-q", "--allow-empty", "-m", "base"]);
        run(&["checkout", "-q", "-b", "other"]);
        run(&["commit", "-q", "--allow-empty", "-m", "only on other"]);
        run(&["checkout", "-q", "main"]);
        let hash = git(root, &["rev-parse", "other"]).trim().to_string();

        let mut app = app(root);
        assert!(app.log_ref.is_none());
        app.focus = Log;
        typed(&mut app, &hash[..9]);
        assert_eq!(app.log_ref.as_deref(), Some("refs/heads/other"));
        assert!(selected(&app, Branches) == Some(Target::Commit("other".into())));
        key(&mut app, KeyCode::Esc);
        assert!(app.lists[Log.idx()].query.is_empty());
        assert!(selected(&app, Log) == Some(Target::Commit(hash[..12].into())));

        // Back on the checked-out branch: the log is HEAD again, working tree on top.
        app.point_branches_at("refs/heads/main");
        app.show_log_of("refs/heads/main", true);
        app.rebuild(Log);
        assert!(app.log_ref.is_none());
        assert!(selected(&app, Log) == Some(Target::Seed));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn masks_credentials_in_properties() {
        assert_eq!(mask_url("origin\thttps://me:tok@github.com/x (fetch)"), "origin\thttps://***@github.com/x (fetch)");
        assert_eq!(mask_url("https://tok@github.com/x"), "https://***@github.com/x");
        assert_eq!(mask_url("ssh://git@host/x"), "ssh://git@host/x");
        assert_eq!(mask_url("git@github.com:me/x"), "git@github.com:me/x");
        assert_eq!(mask("http.https://github.com/.extraheader", "AUTHORIZATION: basic abc"), "***");
        assert_eq!(mask("core.bare", "false"), "false");
    }

    /// Tab switches tabs (never panes); each tab keeps its filter; remote
    /// branches drive the log; a stash shows its patch.
    #[test]
    fn tabs_remote_branches_and_stashes() {
        let dir = std::env::temp_dir().join(format!("gitsmart-tabs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.to_str().unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git").arg("-C").arg(root).args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"]).args(args)
                .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap().success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("f"), "one\n").unwrap();
        run(&["add", "f"]);
        run(&["commit", "-q", "-m", "base"]);
        run(&["update-ref", "refs/remotes/origin/feat", "HEAD"]);
        run(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/feat"]);
        std::fs::write(dir.join("f"), "two\n").unwrap();
        run(&["stash", "-q"]);
        run(&["tag", "v1"]);

        let mut app = app(root);
        assert_eq!(app.data.remotes, ["origin/feat"]); // origin/HEAD left out
        app.focus = Branches;
        key(&mut app, KeyCode::Tab);
        assert!(app.focus == Branches);
        assert_eq!(app.tab[Branches.idx()], 1);
        assert_eq!(app.log_ref.as_deref(), Some("refs/remotes/origin/feat"));
        key(&mut app, KeyCode::Tab);
        assert!(app.log_ref.is_none()); // back on local main = HEAD

        app.focus = Tags;
        typed(&mut app, "v1");
        key(&mut app, KeyCode::Tab);
        assert!(app.lists[STASHES].query.is_empty());
        let stash = selected(&app, Tags).unwrap();
        assert!(stash == Target::Stash("stash@{0}".into()));
        assert!(app.diff.target == stash);
        let text: String = render_diff(root, &stash, 120, false).iter().flat_map(|l| l.spans.iter().map(|s| s.content.to_string())).collect();
        assert!(text.contains("two"), "stash patch missing");
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.lists[Tags.idx()].query, "v1");

        app.focus = Diff;
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.tab[Diff.idx()], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

use std::borrow::Cow;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use arboard::Clipboard;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use pulldown_cmark::{CodeBlockKind, Event as MdEvent, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::prelude::{Alignment, Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Tabs, Wrap,
};
use ratatui::{DefaultTerminal, Frame};
use serde::Deserialize;

const PAGE_SIZE: usize = 20;
const HELP_TEXT: &str = "Tab/Shift+Tab:カテゴリ  j/k,↑/↓:移動  n:PR作成  e:クエリ編集  a:organization  s:状態切替  r:更新  Enter/o:ブラウザ  </>:コピー  Esc/Ctrl+C/Cmd+W/q:終了";

fn main() -> Result<()> {
    if handle_cli_args()? {
        return Ok(());
    }

    let terminal = setup_terminal()?;
    let result = run_app(terminal);
    restore_terminal()?;
    result
}

fn handle_cli_args() -> Result<bool> {
    let mut args = std::env::args().skip(1);
    let Some(arg) = args.next() else {
        return Ok(false);
    };

    match arg.as_str() {
        "--version" | "-V" => {
            println!("ghdesk {}", env!("CARGO_PKG_VERSION"));
            Ok(true)
        }
        "--help" | "-h" => {
            println!(
                "ghdesk {}\n\nUSAGE:\n  ghdesk\n  ghdesk --help\n  ghdesk --version\n\nOPTIONS:\n  -h, --help       Show this help\n  -V, --version    Show version",
                env!("CARGO_PKG_VERSION")
            );
            Ok(true)
        }
        other => Err(anyhow!("unknown argument: {other}")),
    }
}

fn setup_terminal() -> Result<DefaultTerminal> {
    Ok(ratatui::init())
}

fn restore_terminal() -> Result<()> {
    ratatui::restore();
    Ok(())
}

fn run_app(mut terminal: DefaultTerminal) -> Result<()> {
    let (fetch_tx, fetch_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    spawn_fetch_worker(fetch_rx, result_tx);

    let mut app = App::new(fetch_tx, result_rx);
    app.refresh()?;

    loop {
        app.poll_fetch_results();
        terminal.draw(|frame| draw(frame, &mut app))?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if app.handle_key(key)? {
                    break;
                }
            }
            Event::Mouse(mouse) => {
                if matches!(mouse.kind, MouseEventKind::ScrollDown) {
                    if rect_contains(app.preview_rect, mouse.column, mouse.row) {
                        app.scroll_preview_down(3);
                    } else {
                        app.select_next();
                    }
                } else if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                    if rect_contains(app.preview_rect, mouse.column, mouse.row) {
                        app.scroll_preview_up(3);
                    } else {
                        app.select_previous();
                    }
                }
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    AuthoredPr,
    AuthoredIssue,
    AssignedPr,
    AssignedIssue,
}

impl Category {
    const ALL: [Self; 4] = [
        Self::AuthoredPr,
        Self::AuthoredIssue,
        Self::AssignedPr,
        Self::AssignedIssue,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::AuthoredPr => "作成PR",
            Self::AuthoredIssue => "作成Issue",
            Self::AssignedPr => "担当PR",
            Self::AssignedIssue => "担当Issue",
        }
    }

    fn search_query(
        self,
        state: StateFilter,
        organization: Option<&str>,
        extra_query: &str,
    ) -> String {
        let mut parts = vec![match self {
            Self::AuthoredPr => "is:pr author:@me".to_string(),
            Self::AuthoredIssue => "is:issue author:@me".to_string(),
            Self::AssignedPr => "is:pr assignee:@me".to_string(),
            Self::AssignedIssue => "is:issue assignee:@me".to_string(),
        }];

        if let Some(organization) = organization.filter(|value| !value.trim().is_empty()) {
            parts.push(format!("org:{}", organization.trim()));
        }

        if let Some(filter) = state.as_query() {
            parts.push(filter.to_string());
        }

        let trimmed = extra_query.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }

        parts.join(" ")
    }

    fn next(self) -> Self {
        let index = Self::ALL.iter().position(|item| *item == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    fn previous(self) -> Self {
        let index = Self::ALL.iter().position(|item| *item == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateFilter {
    Open,
    Closed,
    All,
}

impl StateFilter {
    fn as_query(self) -> Option<&'static str> {
        match self {
            Self::Open => Some("state:open"),
            Self::Closed => Some("state:closed"),
            Self::All => None,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Open => Self::Closed,
            Self::Closed => Self::All,
            Self::All => Self::Open,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    EditingQuery,
    EditingOrganization,
    CreatingPullRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    PullRequest,
    Issue,
}

impl ItemKind {
    fn label(self) -> &'static str {
        match self {
            Self::PullRequest => "PR",
            Self::Issue => "Issue",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrFormField {
    Title,
    Body,
    Draft,
}

impl PrFormField {
    fn next(self) -> Self {
        match self {
            Self::Title => Self::Body,
            Self::Body => Self::Draft,
            Self::Draft => Self::Title,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Title => Self::Draft,
            Self::Body => Self::Title,
            Self::Draft => Self::Body,
        }
    }
}

#[derive(Debug, Clone)]
struct PullRequestForm {
    title: String,
    body: String,
    draft: bool,
    field: PrFormField,
}

impl Default for PullRequestForm {
    fn default() -> Self {
        Self {
            title: String::new(),
            body: String::new(),
            draft: true,
            field: PrFormField::Title,
        }
    }
}

#[derive(Debug, Clone)]
struct GithubItem {
    kind: ItemKind,
    number: u64,
    title: String,
    url: String,
    state: String,
    repo: String,
    body: String,
    author: String,
    assignees: Vec<String>,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
    is_draft: bool,
}

struct FetchRequest {
    id: u64,
    query: String,
}

struct FetchResponse {
    id: u64,
    query: String,
    result: std::result::Result<Vec<GithubItem>, String>,
}

fn spawn_fetch_worker(fetch_rx: Receiver<FetchRequest>, result_tx: Sender<FetchResponse>) {
    thread::spawn(move || {
        while let Ok(request) = fetch_rx.recv() {
            let result = fetch_items(&request.query).map_err(|err| err.to_string());
            let _ = result_tx.send(FetchResponse {
                id: request.id,
                query: request.query,
                result,
            });
        }
    });
}

impl GithubItem {
    fn summary_line(&self) -> Line<'static> {
        let state_color = if self.state.eq_ignore_ascii_case("open") {
            Color::Green
        } else if self.state.eq_ignore_ascii_case("closed")
            || self.state.eq_ignore_ascii_case("merged")
        {
            Color::Yellow
        } else {
            Color::Cyan
        };

        Line::from(vec![
            Span::styled(
                format!(" {} ", self.kind.label()),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                format!("#{}", self.number),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::raw(self.title.clone()),
            Span::raw(" "),
            Span::styled(
                format!("{} ", self.state.to_uppercase()),
                Style::default()
                    .fg(state_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    }

    fn preview_text(&self) -> Text<'static> {
        let assignees = if self.assignees.is_empty() {
            Cow::Borrowed("なし")
        } else {
            Cow::Owned(self.assignees.join(", "))
        };
        let closed = self.closed_at.as_deref().unwrap_or("-");
        let body = if self.body.trim().is_empty() {
            markdown_to_text("本文なし")
        } else {
            markdown_to_text(&self.body)
        };

        let mut lines = vec![
            Line::styled(
                self.title.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::from(vec![
                Span::styled("種別 ", Style::default().fg(Color::DarkGray)),
                Span::raw(self.kind.label()),
                Span::raw("    "),
                Span::styled("番号 ", Style::default().fg(Color::DarkGray)),
                Span::raw(format!("#{}", self.number)),
            ]),
            Line::from(vec![
                Span::styled("状態 ", Style::default().fg(Color::DarkGray)),
                Span::raw(self.state.clone()),
                Span::raw("    "),
                Span::styled("ドラフト ", Style::default().fg(Color::DarkGray)),
                Span::raw(if self.is_draft { "yes" } else { "no" }),
            ]),
            Line::from(vec![
                Span::styled("Repo ", Style::default().fg(Color::DarkGray)),
                Span::raw(self.repo.clone()),
            ]),
            Line::from(vec![
                Span::styled("作成者 ", Style::default().fg(Color::DarkGray)),
                Span::raw(self.author.clone()),
            ]),
            Line::from(vec![
                Span::styled("担当者 ", Style::default().fg(Color::DarkGray)),
                Span::raw(assignees.into_owned()),
            ]),
            Line::from(vec![
                Span::styled("作成 ", Style::default().fg(Color::DarkGray)),
                Span::raw(self.created_at.clone()),
            ]),
            Line::from(vec![
                Span::styled("更新 ", Style::default().fg(Color::DarkGray)),
                Span::raw(self.updated_at.clone()),
            ]),
            Line::from(vec![
                Span::styled("終了 ", Style::default().fg(Color::DarkGray)),
                Span::raw(closed.to_string()),
            ]),
            Line::from(vec![
                Span::styled("URL  ", Style::default().fg(Color::DarkGray)),
                Span::raw(self.url.clone()),
            ]),
            Line::raw(""),
            Line::styled(
                "Preview",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
        ];
        lines.extend(body.lines);
        Text::from(lines)
    }
}

fn markdown_to_text(markdown: &str) -> Text<'static> {
    let parser = Parser::new_ext(markdown, Options::all());
    let mut renderer = MarkdownRenderer::default();
    for event in parser {
        renderer.push(event);
    }
    renderer.finish()
}

#[derive(Default)]
struct MarkdownRenderer {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    list_stack: Vec<Option<u64>>,
    blockquote_depth: usize,
    code_block: bool,
    pending_link: Option<String>,
}

impl MarkdownRenderer {
    fn push(&mut self, event: MdEvent<'_>) {
        match event {
            MdEvent::Start(tag) => self.start_tag(tag),
            MdEvent::End(tag) => self.end_tag(tag),
            MdEvent::Text(text) => self.push_text(&text),
            MdEvent::Code(text) => {
                self.current.push(Span::styled(
                    text.into_string(),
                    Style::default()
                        .fg(Color::Yellow)
                        .bg(Color::Rgb(24, 28, 40)),
                ));
            }
            MdEvent::SoftBreak => self.current.push(Span::raw(" ")),
            MdEvent::HardBreak => self.flush_line(),
            MdEvent::Rule => {
                self.flush_line();
                self.lines.push(Line::styled(
                    "────────────────",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush_line();
                let style = match level {
                    HeadingLevel::H1 => Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                    HeadingLevel::H2 => Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                    _ => Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                };
                self.style_stack.push(style);
            }
            Tag::Emphasis => self
                .style_stack
                .push(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self
                .style_stack
                .push(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .style_stack
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.code_block = true;
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => format!("code: {}", lang),
                    _ => "code".to_string(),
                };
                self.lines.push(Line::styled(
                    lang,
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            Tag::List(start) => {
                self.flush_line();
                self.list_stack.push(start);
            }
            Tag::Item => {
                self.flush_line();
                let indent = "  ".repeat(self.list_stack.len().saturating_sub(1));
                let bullet = if let Some(Some(num)) = self.list_stack.last_mut() {
                    let current = *num;
                    *num += 1;
                    format!("{indent}{current}. ")
                } else {
                    format!("{indent}• ")
                };
                self.current
                    .push(Span::styled(bullet, Style::default().fg(Color::Cyan)));
            }
            Tag::Link { dest_url, .. } => {
                self.pending_link = Some(dest_url.into_string());
                self.style_stack.push(
                    Style::default()
                        .fg(Color::LightBlue)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                self.lines.push(Line::raw(""));
            }
            TagEnd::Heading(_) => {
                self.style_stack.pop();
                self.flush_line();
                self.lines.push(Line::raw(""));
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.style_stack.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.lines.push(Line::raw(""));
            }
            TagEnd::CodeBlock => {
                self.code_block = false;
                self.flush_line();
                self.lines.push(Line::raw(""));
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                self.flush_line();
                self.lines.push(Line::raw(""));
            }
            TagEnd::Item => self.flush_line(),
            TagEnd::Link => {
                self.style_stack.pop();
                if let Some(url) = self.pending_link.take() {
                    self.current.push(Span::styled(
                        format!(" ({url})"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            }
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        let style = if self.code_block {
            self.combined_style()
                .fg(Color::Yellow)
                .bg(Color::Rgb(24, 28, 40))
        } else {
            self.combined_style()
        };

        let segments: Vec<&str> = text.split('\n').collect();
        for (index, segment) in segments.iter().enumerate() {
            if self.current.is_empty() && self.blockquote_depth > 0 {
                self.current.push(Span::styled(
                    format!("{} ", ">".repeat(self.blockquote_depth)),
                    Style::default().fg(Color::Green),
                ));
            }
            if self.current.is_empty() && self.code_block {
                self.current.push(Span::styled(
                    "  ",
                    Style::default().bg(Color::Rgb(24, 28, 40)),
                ));
            }

            if !segment.is_empty() {
                self.current
                    .push(Span::styled((*segment).to_string(), style));
            }

            if index + 1 < segments.len() {
                self.flush_line();
            }
        }
    }

    fn combined_style(&self) -> Style {
        self.style_stack
            .iter()
            .copied()
            .fold(Style::default(), |style, item| style.patch(item))
    }

    fn flush_line(&mut self) {
        let line = if self.current.is_empty() {
            Line::raw("")
        } else {
            Line::from(std::mem::take(&mut self.current))
        };
        self.lines.push(line);
    }

    fn finish(mut self) -> Text<'static> {
        if !self.current.is_empty() {
            self.flush_line();
        }
        if self.lines.is_empty() {
            self.lines.push(Line::raw(""));
        }
        Text::from(self.lines)
    }
}

struct App {
    category: Category,
    state_filter: StateFilter,
    organization: String,
    query: String,
    organization_buffer: String,
    query_buffer: String,
    input_mode: InputMode,
    pr_form: PullRequestForm,
    fetch_tx: Sender<FetchRequest>,
    result_rx: Receiver<FetchResponse>,
    request_id: u64,
    active_request_id: Option<u64>,
    loading: bool,
    items: Vec<GithubItem>,
    selected: usize,
    preview_scroll: u16,
    preview_rect: Rect,
    status: String,
    last_query: String,
}

impl App {
    fn new(fetch_tx: Sender<FetchRequest>, result_rx: Receiver<FetchResponse>) -> Self {
        Self {
            category: Category::AuthoredPr,
            state_filter: StateFilter::Open,
            organization: String::new(),
            query: String::new(),
            organization_buffer: String::new(),
            query_buffer: String::new(),
            input_mode: InputMode::Normal,
            pr_form: PullRequestForm::default(),
            fetch_tx,
            result_rx,
            request_id: 0,
            active_request_id: None,
            loading: false,
            items: Vec::new(),
            selected: 0,
            preview_scroll: 0,
            preview_rect: Rect::default(),
            status: "起動中…".to_string(),
            last_query: String::new(),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.input_mode == InputMode::EditingQuery {
            return self.handle_query_key(key);
        }
        if self.input_mode == InputMode::EditingOrganization {
            return self.handle_organization_key(key);
        }
        if self.input_mode == InputMode::CreatingPullRequest {
            return self.handle_pr_form_key(key);
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::SUPER) => return Ok(true),
            KeyCode::Char('n') => {
                self.input_mode = InputMode::CreatingPullRequest;
                self.pr_form = PullRequestForm::default();
                self.status =
                    "PR 作成画面を開きました。Tab で移動、Ctrl+S で作成します".to_string();
            }
            KeyCode::Tab => self.switch_category(self.category.next())?,
            KeyCode::BackTab => self.switch_category(self.category.previous())?,
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_previous(),
            KeyCode::PageDown => self.scroll_preview_down(10),
            KeyCode::PageUp => self.scroll_preview_up(10),
            KeyCode::Char('J') => self.scroll_preview_down(3),
            KeyCode::Char('K') => self.scroll_preview_up(3),
            KeyCode::Char('g') | KeyCode::Home => self.selected = 0,
            KeyCode::Char('G') | KeyCode::End => self.selected = self.items.len().saturating_sub(1),
            KeyCode::Char('e') | KeyCode::Char('/') => {
                self.input_mode = InputMode::EditingQuery;
                self.query_buffer = self.query.clone();
                self.status = "クエリ編集中。Enter で適用、Esc でキャンセル".to_string();
            }
            KeyCode::Char('a') => {
                self.input_mode = InputMode::EditingOrganization;
                self.organization_buffer = self.organization.clone();
                self.status =
                    "organization 編集中。空にすると全 organization が対象です".to_string();
            }
            KeyCode::Char('s') => {
                self.state_filter = self.state_filter.next();
                self.refresh()?;
            }
            KeyCode::Char('r') => self.refresh()?,
            KeyCode::Enter | KeyCode::Char('o') => self.open_selected_in_browser()?,
            KeyCode::Char('<') | KeyCode::Char(',') if is_copy_url_shortcut(key) => {
                self.copy_selected_url()?;
            }
            KeyCode::Char('>') | KeyCode::Char('.') if is_copy_number_shortcut(key) => {
                self.copy_selected_number()?;
            }
            _ => {}
        }

        Ok(false)
    }

    fn handle_pr_form_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('s')) {
            return self.submit_pull_request();
        }

        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.status = "PR 作成をキャンセルしました".to_string();
            }
            KeyCode::Tab => {
                self.pr_form.field = self.pr_form.field.next();
            }
            KeyCode::BackTab => {
                self.pr_form.field = self.pr_form.field.previous();
            }
            KeyCode::Up => {
                self.pr_form.field = self.pr_form.field.previous();
            }
            KeyCode::Down => {
                self.pr_form.field = self.pr_form.field.next();
            }
            KeyCode::Enter => match self.pr_form.field {
                PrFormField::Title => self.pr_form.field = PrFormField::Body,
                PrFormField::Body => self.pr_form.body.push('\n'),
                PrFormField::Draft => self.pr_form.draft = !self.pr_form.draft,
            },
            KeyCode::Backspace => match self.pr_form.field {
                PrFormField::Title => {
                    self.pr_form.title.pop();
                }
                PrFormField::Body => {
                    self.pr_form.body.pop();
                }
                PrFormField::Draft => {}
            },
            KeyCode::Char(' ') if self.pr_form.field == PrFormField::Draft => {
                self.pr_form.draft = !self.pr_form.draft;
            }
            KeyCode::Char(ch) => match self.pr_form.field {
                PrFormField::Title => self.pr_form.title.push(ch),
                PrFormField::Body => self.pr_form.body.push(ch),
                PrFormField::Draft => {
                    if matches!(ch, 'x' | 'X' | 'd' | 'D') {
                        self.pr_form.draft = !self.pr_form.draft;
                    }
                }
            },
            _ => {}
        }

        Ok(false)
    }

    fn handle_organization_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.organization_buffer.clear();
                self.status = "organization 編集をキャンセルしました".to_string();
            }
            KeyCode::Enter => {
                self.organization = self.organization_buffer.trim().to_string();
                self.input_mode = InputMode::Normal;
                self.refresh()?;
            }
            KeyCode::Backspace => {
                self.organization_buffer.pop();
            }
            KeyCode::Char(ch) => {
                self.organization_buffer.push(ch);
            }
            _ => {}
        }

        Ok(false)
    }

    fn handle_query_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.query_buffer.clear();
                self.status = "クエリ編集をキャンセルしました".to_string();
            }
            KeyCode::Enter => {
                self.query = self.query_buffer.trim().to_string();
                self.input_mode = InputMode::Normal;
                self.refresh()?;
            }
            KeyCode::Backspace => {
                self.query_buffer.pop();
            }
            KeyCode::Char(ch) => {
                self.query_buffer.push(ch);
            }
            _ => {}
        }

        Ok(false)
    }

    fn switch_category(&mut self, category: Category) -> Result<()> {
        self.category = category;
        self.refresh()
    }

    fn refresh(&mut self) -> Result<()> {
        let search_query = self.category.search_query(
            self.state_filter,
            (!self.organization.is_empty()).then_some(self.organization.as_str()),
            &self.query,
        );
        self.request_id += 1;
        self.active_request_id = Some(self.request_id);
        self.loading = true;
        self.status = "GitHub から取得中…".to_string();
        self.last_query = search_query.clone();
        self.fetch_tx
            .send(FetchRequest {
                id: self.request_id,
                query: search_query,
            })
            .map_err(|_| anyhow!("検索ワーカーへリクエストを送信できませんでした"))?;
        Ok(())
    }

    fn poll_fetch_results(&mut self) {
        while let Ok(response) = self.result_rx.try_recv() {
            if Some(response.id) != self.active_request_id {
                continue;
            }

            self.loading = false;
            self.active_request_id = None;
            self.last_query = response.query;
            match response.result {
                Ok(items) => {
                    self.items = items;
                    self.selected = self.selected.min(self.items.len().saturating_sub(1));
                    self.preview_scroll = 0;
                    self.status = if self.items.is_empty() {
                        "一致する項目はありません".to_string()
                    } else {
                        format!("{} 件取得", self.items.len())
                    };
                }
                Err(error) => {
                    self.items.clear();
                    self.selected = 0;
                    self.status = error;
                }
            }
        }
    }

    fn submit_pull_request(&mut self) -> Result<bool> {
        let title = self.pr_form.title.trim();
        if title.is_empty() {
            self.status = "PR タイトルは必須です".to_string();
            return Ok(false);
        }

        self.status = "Pull Request を作成中…".to_string();
        let url = create_pull_request(title, &self.pr_form.body, self.pr_form.draft)?;
        self.input_mode = InputMode::Normal;
        self.status = format!("Pull Request を作成しました: {url}");
        Ok(false)
    }

    fn selected_item(&self) -> Option<&GithubItem> {
        self.items.get(self.selected)
    }

    fn select_next(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
        self.preview_scroll = 0;
    }

    fn select_previous(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.items.len() - 1
        } else {
            self.selected - 1
        };
        self.preview_scroll = 0;
    }

    fn scroll_preview_down(&mut self, amount: u16) {
        self.preview_scroll = self.preview_scroll.saturating_add(amount);
    }

    fn scroll_preview_up(&mut self, amount: u16) {
        self.preview_scroll = self.preview_scroll.saturating_sub(amount);
    }

    fn open_selected_in_browser(&mut self) -> Result<()> {
        let item = self
            .selected_item()
            .ok_or_else(|| anyhow!("項目が選択されていません"))?;
        open_in_browser(&item.url)?;
        self.status = format!("ブラウザで開きました: {}", item.url);
        Ok(())
    }

    fn copy_selected_url(&mut self) -> Result<()> {
        let item = self
            .selected_item()
            .ok_or_else(|| anyhow!("項目が選択されていません"))?;
        copy_to_clipboard(&item.url)?;
        self.status = format!("URL をコピーしました: {}", item.url);
        Ok(())
    }

    fn copy_selected_number(&mut self) -> Result<()> {
        let item = self
            .selected_item()
            .ok_or_else(|| anyhow!("項目が選択されていません"))?;
        copy_to_clipboard(&item.number.to_string())?;
        self.status = format!("#{} をコピーしました", item.number);
        Ok(())
    }
}

fn is_copy_url_shortcut(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('<'))
        || (matches!(key.code, KeyCode::Char(',')) && key.modifiers.contains(KeyModifiers::SHIFT))
        || (matches!(key.code, KeyCode::Char(',')) && key.modifiers.contains(KeyModifiers::SUPER))
}

fn is_copy_number_shortcut(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('>'))
        || (matches!(key.code, KeyCode::Char('.')) && key.modifiers.contains(KeyModifiers::SHIFT))
        || (matches!(key.code, KeyCode::Char('.')) && key.modifiers.contains(KeyModifiers::SUPER))
}

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let background = Block::new().style(Style::default().bg(Color::Rgb(10, 14, 24)));
    frame.render_widget(background, area);

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(4),
            Constraint::Length(4),
            Constraint::Min(10),
            Constraint::Length(2),
        ])
        .split(area);

    draw_tabs(frame, vertical[0], app);
    draw_filters(frame, vertical[1], app);
    draw_scope(frame, vertical[2], app);
    draw_content(frame, vertical[3], app);
    draw_status(frame, vertical[4], app);

    if app.input_mode == InputMode::EditingQuery {
        draw_query_modal(frame, area, app);
    } else if app.input_mode == InputMode::EditingOrganization {
        draw_organization_modal(frame, area, app);
    } else if app.input_mode == InputMode::CreatingPullRequest {
        draw_pr_modal(frame, area, app);
    }
}

fn draw_tabs(frame: &mut Frame, area: Rect, app: &App) {
    let titles: Vec<Line<'_>> = Category::ALL
        .iter()
        .map(|category| Line::from(format!(" {} ", category.title())))
        .collect();

    let tabs = Tabs::new(titles)
        .select(
            Category::ALL
                .iter()
                .position(|category| *category == app.category)
                .unwrap_or(0),
        )
        .block(
            Block::bordered()
                .border_set(border::THICK)
                .title(" GitHub Workbench ")
                .title_alignment(Alignment::Center)
                .style(
                    Style::default()
                        .fg(Color::Rgb(181, 201, 255))
                        .bg(Color::Rgb(18, 24, 38)),
                ),
        )
        .style(Style::default().fg(Color::Gray))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(108, 233, 255))
                .add_modifier(Modifier::BOLD),
        )
        .divider(" ");
    frame.render_widget(tabs, area);
}

fn draw_filters(frame: &mut Frame, area: Rect, app: &App) {
    let sections = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(24), Constraint::Min(20)])
        .split(area);

    let state_line = Line::from(vec![
        chip("Open", app.state_filter == StateFilter::Open, Color::Green),
        Span::raw(" "),
        chip(
            "Closed",
            app.state_filter == StateFilter::Closed,
            Color::Yellow,
        ),
        Span::raw(" "),
        chip("All", app.state_filter == StateFilter::All, Color::Cyan),
    ]);

    let state = Paragraph::new(state_line).block(
        Block::bordered()
            .title(" State ")
            .border_style(Style::default().fg(Color::Rgb(70, 88, 128))),
    );
    frame.render_widget(state, sections[0]);

    let query = Paragraph::new(app.query.as_str()).block(
        Block::bordered()
            .title(" Filter Query (`e` で編集) ")
            .border_style(Style::default().fg(Color::Rgb(70, 88, 128))),
    );
    frame.render_widget(query, sections[1]);
}

fn draw_scope(frame: &mut Frame, area: Rect, app: &App) {
    let sections = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(36), Constraint::Min(20)])
        .split(area);

    let organization = if app.organization.is_empty() {
        "All organizations".to_string()
    } else {
        format!("org:{}", app.organization)
    };

    let scope = Paragraph::new(organization).block(
        Block::bordered()
            .title(" Organization (`a` で編集) ")
            .border_style(Style::default().fg(Color::Rgb(70, 88, 128))),
    );
    frame.render_widget(scope, sections[0]);

    let query = Paragraph::new(app.last_query.as_str()).block(
        Block::bordered()
            .title(" Effective Search ")
            .border_style(Style::default().fg(Color::Rgb(70, 88, 128))),
    );
    frame.render_widget(query, sections[1]);
}

fn draw_content(frame: &mut Frame, area: Rect, app: &mut App) {
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
        .split(area);

    let items: Vec<ListItem<'_>> = if app.items.is_empty() {
        vec![ListItem::new(Line::styled(
            "項目がありません",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.items
            .iter()
            .map(|item| {
                let repo = Line::styled(
                    format!("{}  {}", item.repo, item.updated_at),
                    Style::default().fg(Color::DarkGray),
                );
                ListItem::new(vec![item.summary_line(), repo])
            })
            .collect()
    };

    let list_title = if app.loading {
        format!(" Results ({}) • Loading… ", app.items.len())
    } else {
        format!(" Results ({}) ", app.items.len())
    };

    let list = List::new(items)
        .block(
            Block::bordered()
                .title(list_title)
                .border_style(Style::default().fg(Color::Rgb(70, 88, 128))),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(30, 42, 66))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▌ ");

    let mut list_state = ListState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(list, panes[0], &mut list_state);

    app.preview_rect = panes[1];
    frame.render_widget(Clear, panes[1]);

    let preview = if let Some(item) = app.selected_item() {
        Paragraph::new(item.preview_text())
            .scroll((app.preview_scroll, 0))
            .wrap(Wrap { trim: false })
            .block(
                Block::bordered()
                    .title(" Preview ")
                    .border_style(Style::default().fg(Color::Rgb(70, 88, 128)))
                    .padding(Padding::horizontal(1)),
            )
    } else {
        Paragraph::new("選択中の項目はありません").block(
            Block::bordered()
                .title(" Preview ")
                .border_style(Style::default().fg(Color::Rgb(70, 88, 128))),
        )
    };
    frame.render_widget(preview, panes[1]);
}

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let status_text = if app.loading {
        format!("{}  query: {}", app.status, app.last_query)
    } else {
        app.status.clone()
    };

    let status = Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled("STATUS ", Style::default().fg(Color::Rgb(108, 233, 255))),
            Span::raw(status_text),
        ]),
        Line::styled(HELP_TEXT, Style::default().fg(Color::DarkGray)),
    ]))
    .block(
        Block::new()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::Rgb(70, 88, 128))),
    );
    frame.render_widget(status, area);
}

fn draw_query_modal(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(72, 7, area);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Search Query ")
        .border_set(border::ROUNDED)
        .style(
            Style::default()
                .fg(Color::Rgb(181, 201, 255))
                .bg(Color::Rgb(18, 24, 38)),
        );
    frame.render_widget(block, popup);

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(popup);

    frame.render_widget(
        Paragraph::new(
            "GitHub 検索クエリを追加できます。例: repo:owner/name label:bug sort:updated-desc",
        ),
        inner[0],
    );
    frame.render_widget(
        Paragraph::new(app.query_buffer.as_str()).style(Style::default().fg(Color::White)),
        inner[1],
    );
    frame.render_widget(
        Paragraph::new("Enter:適用  Esc:キャンセル").style(Style::default().fg(Color::DarkGray)),
        inner[2],
    );
    frame.set_cursor_position((inner[1].x + app.query_buffer.len() as u16, inner[1].y));
}

fn draw_organization_modal(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(60, 7, area);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Organization Filter ")
        .border_set(border::ROUNDED)
        .style(
            Style::default()
                .fg(Color::Rgb(181, 201, 255))
                .bg(Color::Rgb(18, 24, 38)),
        );
    frame.render_widget(block, popup);

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(popup);

    frame.render_widget(
        Paragraph::new("organization 名を入力してください。空欄なら全 organization が対象です。例: matsumoto-ops"),
        inner[0],
    );
    frame.render_widget(
        Paragraph::new(app.organization_buffer.as_str()).style(Style::default().fg(Color::White)),
        inner[1],
    );
    frame.render_widget(
        Paragraph::new("Enter:適用  Esc:キャンセル").style(Style::default().fg(Color::DarkGray)),
        inner[2],
    );
    frame.set_cursor_position((
        inner[1].x + app.organization_buffer.len() as u16,
        inner[1].y,
    ));
}

fn draw_pr_modal(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(72, 16, area);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Create Pull Request ")
        .border_set(border::ROUNDED)
        .style(
            Style::default()
                .fg(Color::Rgb(181, 201, 255))
                .bg(Color::Rgb(18, 24, 38)),
        );
    frame.render_widget(block, popup);

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(popup);

    frame.render_widget(
        Paragraph::new(
            "タイトル / 本文 / Draft を編集して Ctrl+S で作成します。Esc でキャンセル。",
        ),
        inner[0],
    );

    frame.render_widget(
        Paragraph::new(app.pr_form.title.as_str())
            .style(active_field_style(app.pr_form.field == PrFormField::Title))
            .block(
                Block::bordered()
                    .title(" Title ")
                    .border_style(active_border_style(app.pr_form.field == PrFormField::Title)),
            ),
        inner[1],
    );

    frame.render_widget(
        Paragraph::new(app.pr_form.body.as_str())
            .wrap(Wrap { trim: false })
            .style(active_field_style(app.pr_form.field == PrFormField::Body))
            .block(
                Block::bordered()
                    .title(" Body ")
                    .border_style(active_border_style(app.pr_form.field == PrFormField::Body)),
            ),
        inner[2],
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("Draft "),
            chip("ON", app.pr_form.draft, Color::Yellow),
            Span::raw(" "),
            chip("OFF", !app.pr_form.draft, Color::DarkGray),
        ]))
        .block(
            Block::bordered()
                .title(" Draft ")
                .border_style(active_border_style(app.pr_form.field == PrFormField::Draft)),
        ),
        inner[3],
    );

    frame.render_widget(
        Paragraph::new(
            "Tab/Shift+Tab:移動  Enter:本文改行またはDraft切替  Space:Draft切替  Ctrl+S:作成",
        )
        .style(Style::default().fg(Color::DarkGray)),
        inner[4],
    );

    match app.pr_form.field {
        PrFormField::Title => {
            frame.set_cursor_position((
                inner[1].x + 1 + app.pr_form.title.chars().count() as u16,
                inner[1].y + 1,
            ));
        }
        PrFormField::Body => {
            let (line, col) = cursor_for_multiline(&app.pr_form.body);
            frame.set_cursor_position((inner[2].x + 1 + col as u16, inner[2].y + 1 + line as u16));
        }
        PrFormField::Draft => {}
    }
}

fn chip<'a>(label: &'a str, active: bool, color: Color) -> Span<'a> {
    if active {
        Span::styled(
            format!(" {} ", label),
            Style::default()
                .fg(Color::Black)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            format!(" {} ", label),
            Style::default().fg(color).bg(Color::Rgb(22, 28, 44)),
        )
    }
}

fn active_border_style(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::Rgb(70, 88, 128))
    }
}

fn active_field_style(active: bool) -> Style {
    if active {
        Style::default().fg(Color::White).bg(Color::Rgb(30, 42, 66))
    } else {
        Style::default().fg(Color::White)
    }
}

fn centered_rect(width_percent: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(height),
            Constraint::Min(1),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn cursor_for_multiline(text: &str) -> (usize, usize) {
    let mut line = 0;
    let mut col = 0;
    for ch in text.chars() {
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn fetch_items(search_query: &str) -> Result<Vec<GithubItem>> {
    let graphql = r#"
query($searchQuery: String!, $first: Int!) {
  search(query: $searchQuery, type: ISSUE, first: $first) {
    nodes {
      __typename
      ... on PullRequest {
        number
        title
        url
        state
        isDraft
        bodyText
        createdAt
        updatedAt
        closedAt
        repository { nameWithOwner }
        author { login }
        assignees(first: 10) { nodes { login } }
      }
      ... on Issue {
        number
        title
        url
        state
        bodyText
        createdAt
        updatedAt
        closedAt
        repository { nameWithOwner }
        author { login }
        assignees(first: 10) { nodes { login } }
      }
    }
  }
}
"#;

    let output = Command::new("gh")
        .arg("api")
        .arg("graphql")
        .arg("-f")
        .arg(format!("query={graphql}"))
        .arg("-F")
        .arg(format!("searchQuery={search_query}"))
        .arg("-F")
        .arg(format!("first={PAGE_SIZE}"))
        .output()
        .context("gh api graphql の実行に失敗しました")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("GitHub 検索に失敗しました: {}", stderr.trim()));
    }

    let response: GraphqlResponse =
        serde_json::from_slice(&output.stdout).context("GitHub の応答を解析できませんでした")?;
    if let Some(errors) = response.errors {
        let message = errors
            .into_iter()
            .map(|error| error.message)
            .collect::<Vec<_>>()
            .join(" / ");
        return Err(anyhow!("GitHub API エラー: {message}"));
    }

    let mut items = Vec::new();
    for node in response.data.search.nodes {
        match node {
            SearchNode::PullRequest(node) => items.push(node.into_item(ItemKind::PullRequest)),
            SearchNode::Issue(node) => items.push(node.into_item(ItemKind::Issue)),
            SearchNode::Unknown => {}
        }
    }
    Ok(items)
}

fn open_in_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(url);
        command
    };

    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    };

    let status = command
        .status()
        .context("ブラウザ起動コマンドの実行に失敗しました")?;
    if !status.success() {
        return Err(anyhow!("ブラウザを開けませんでした"));
    }
    Ok(())
}

fn create_pull_request(title: &str, body: &str, draft: bool) -> Result<String> {
    let mut command = Command::new("gh");
    command
        .arg("pr")
        .arg("create")
        .arg("--title")
        .arg(title)
        .arg("--body")
        .arg(body);
    if draft {
        command.arg("--draft");
    }

    let output = command
        .output()
        .context("gh pr create の実行に失敗しました")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Pull Request を作成できませんでした: {}",
            stderr.trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let url = stdout
        .lines()
        .rev()
        .find(|line| line.starts_with("https://"))
        .map(str::to_string)
        .unwrap_or_else(|| stdout.trim().to_string());
    Ok(url)
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    if let Ok(mut clipboard) = Clipboard::new() {
        clipboard
            .set_text(text.to_string())
            .context("クリップボードへ書き込めませんでした")?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let mut child = Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("pbcopy を起動できませんでした")?;
        use std::io::Write;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(text.as_bytes())?;
        }
        let status = child.wait()?;
        if status.success() {
            return Ok(());
        }
    }

    Err(anyhow!("クリップボードにコピーできませんでした"))
}

#[derive(Debug, Deserialize)]
struct GraphqlResponse {
    data: GraphqlData,
    errors: Option<Vec<GraphqlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphqlData {
    search: SearchResult,
}

#[derive(Debug, Deserialize)]
struct SearchResult {
    nodes: Vec<SearchNode>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum SearchNode {
    PullRequest(GraphqlItem),
    Issue(GraphqlItem),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct GraphqlItem {
    number: u64,
    title: String,
    url: String,
    state: String,
    #[serde(rename = "bodyText")]
    body_text: String,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    #[serde(rename = "closedAt")]
    closed_at: Option<String>,
    #[serde(default, rename = "isDraft")]
    is_draft: bool,
    repository: GraphqlRepository,
    author: Option<GraphqlActor>,
    assignees: GraphqlActors,
}

impl GraphqlItem {
    fn into_item(self, kind: ItemKind) -> GithubItem {
        GithubItem {
            kind,
            number: self.number,
            title: self.title,
            url: self.url,
            state: self.state,
            repo: self.repository.name_with_owner,
            body: self.body_text,
            author: self
                .author
                .map(|author| author.login)
                .unwrap_or_else(|| "-".to_string()),
            assignees: self
                .assignees
                .nodes
                .into_iter()
                .map(|actor| actor.login)
                .collect(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            closed_at: self.closed_at,
            is_draft: self.is_draft,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GraphqlRepository {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

#[derive(Debug, Deserialize)]
struct GraphqlActors {
    nodes: Vec<GraphqlActor>,
}

#[derive(Debug, Deserialize)]
struct GraphqlActor {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GraphqlError {
    message: String,
}

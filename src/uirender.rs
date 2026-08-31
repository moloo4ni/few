use crate::agent::NoticeLevel;
use crate::app::{label_mode, App};
use crate::commands::{arg_options, filter_commands, find_command};
use crate::markdown::{self, MarkdownLine};
use crate::theme;
use crate::transcript::{
    Block, Expand, Hit, PermAskBlock, ResumedItem, ResumedSession, StepItem, StepsGroup,
    PERM_OPTIONS,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;

type Seg = (String, Style);

fn wrap_segments(segs: &[Seg], width: usize) -> Vec<Vec<Seg>> {
    let mut rows: Vec<Vec<Seg>> = vec![Vec::new()];
    let mut row_w = 0usize;

    /// Append a run of text, merging it into the previous segment when the style
    /// matches. Over-long runs are allowed to overflow (and clip at the buffer)
    /// rather than being broken mid-word.
    // This helper appends rows as wrapping proceeds, so it intentionally needs
    // the growable Vec rather than a slice.
    #[allow(clippy::ptr_arg)]
    fn push_run(rows: &mut Vec<Vec<Seg>>, row_w: &mut usize, s: &str, st: Style) {
        if s.is_empty() {
            return;
        }
        let w: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
        let row = rows.last_mut().unwrap();
        match row.last_mut() {
            Some((t, s2)) if *s2 == st => t.push_str(s),
            _ => row.push((s.to_string(), st)),
        }
        *row_w += w;
    }

    /// Place one whitespace-delimited word, breaking only before it (never in
    /// the middle of it). A word longer than the whole line overflows intact.
    fn place_word(
        rows: &mut Vec<Vec<Seg>>,
        row_w: &mut usize,
        word: &str,
        st: Style,
        width: usize,
    ) {
        let wlen: usize = word.chars().map(|c| c.width().unwrap_or(0)).sum();
        if *row_w == 0 {
            push_run(rows, row_w, word, st);
        } else if *row_w + 1 + wlen <= width {
            push_run(rows, row_w, &format!(" {word}"), st);
        } else {
            rows.push(Vec::new());
            *row_w = 0;
            push_run(rows, row_w, word, st);
        }
    }

    for (text, st) in segs {
        let mut word = String::new();
        for c in text.chars() {
            if c.is_whitespace() {
                if !word.is_empty() {
                    place_word(&mut rows, &mut row_w, &word, *st, width);
                    word.clear();
                }
            } else {
                word.push(c);
            }
        }
        if !word.is_empty() {
            place_word(&mut rows, &mut row_w, &word, *st, width);
        }
    }

    while rows.len() > 1 && rows.last().map(|r| r.is_empty()).unwrap_or(false) {
        rows.pop();
    }
    if rows.iter().all(|r| r.is_empty()) {
        rows = vec![vec![(String::new(), theme::normal())]];
    }
    rows
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let width = area.width as usize;

    let input_rows = build_input_rows(app, width.max(4));
    let input_h = input_rows.rows.len().clamp(1, 6) as u16;
    // two rows while busy: a leading blank separates the live thinking/working
    // indicator from the transcript above, so the user prompt and the indicator
    // are clearly spaced instead of crammed onto adjacent lines
    let busy_rows: u16 = if app.running { 2 } else { 0 };
    let pal_items = palette_items(app);
    let pal_rows = pal_items
        .as_ref()
        .map(|v| v.len().min(8) as u16)
        .unwrap_or(0);

    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(busy_rows),
        Constraint::Length(pal_rows),
        Constraint::Length(1), // blank line above the input area
        Constraint::Length(input_h),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    let transcript = chunks[0];
    app.transcript_area = transcript;

    let all_rows = build_rows(app, width.max(4));
    let total = all_rows.len();
    // anchor manual scroll: while the user is scrolled up (offset > 0), new
    // transcript rows must not drag the viewport back toward the live edge
    if app.scroll_from_end > 0 {
        let delta = total as isize - app.scroll_total_seen as isize;
        if delta != 0 {
            app.scroll_from_end = (app.scroll_from_end as isize + delta).max(0) as usize;
        }
    }
    app.scroll_total_seen = total;
    let h = transcript.height as usize;
    let offset = app.scroll_from_end.min(total.saturating_sub(h));
    let start = total
        .saturating_sub(offset + h)
        .min(total.saturating_sub(h));
    let end = (start + h).min(total);
    let visible = &all_rows[start..end];

    let mut hits = vec![Hit::Nothing; h];
    let mut lines: Vec<Line> = Vec::with_capacity(h);
    for (i, (segs, hit)) in visible.iter().enumerate() {
        hits[i] = *hit;
        lines.push(Line::from(
            segs.iter()
                .map(|(t, s)| Span::styled(t.clone(), *s))
                .collect::<Vec<_>>(),
        ));
    }

    // Bottom-anchor short transcripts: at the live edge, when the content is
    // shorter than the area, pad blank lines above so new output grows upward
    // from the input instead of hugging the top. Once the area fills, the tail
    // is shown and older rows scroll off the top (handled by the start/end slice).
    let n = visible.len();
    if app.scroll_from_end == 0 && n < h {
        let pad = h - n;
        let blank = Line::default();
        let mut padded_lines = vec![blank; pad];
        padded_lines.extend(lines);
        lines = padded_lines;
        let mut padded_hits = vec![Hit::Nothing; h];
        padded_hits[pad..].copy_from_slice(&hits[..n]);
        hits = padded_hits;
    }

    app.hitmap = hits;
    f.render_widget(Paragraph::new(lines), transcript);

    if busy_rows > 0 {
        let mut text = if let Some(t) = app.thinking_since {
            format!("thinking · {}s", t.elapsed().as_secs())
        } else {
            let secs = app.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            format!("working · {secs}s")
        };
        if !app.input.is_empty() {
            text += " · typed text queued";
        }
        // leading blank row separates this from the transcript above; the line
        // itself is dim so it reads as transient status, not a log entry
        f.render_widget(
            Paragraph::new(vec![
                Line::default(),
                Line::from(Span::styled(text, theme::dim())),
            ]),
            chunks[1],
        );
    }

    if let (Some(items), true) = (&pal_items, pal_rows > 0) {
        render_palette(f, chunks[2], items, app.palette_sel);
    }

    render_input(f, chunks[4], app, input_rows);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled("─".repeat(width), theme::dim()))),
        chunks[5],
    );

    render_status(f, chunks[6], app);
}

fn render_status(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    // percentage of the context window used, rounded to the nearest percent so
    // a small task (e.g. 1.6k / 200k) shows "1%" instead of flooring to "0%"
    let pct = if app.ctx_window > 0 {
        let used = app.ctx_used.min(app.ctx_window);
        ((used.saturating_mul(100) + app.ctx_window / 2) / app.ctx_window) as usize
    } else {
        0
    };
    let spans = vec![
        Span::styled("model:", theme::dim()),
        Span::raw(format!(" {}    ", app.model_name)),
        Span::styled("mode:", theme::dim()),
        Span::raw(format!(" {}    ", mode_label(app.mode))),
        Span::styled("ctx:", theme::dim()),
        Span::raw(format!(
            " {} / {} ({pct}%)",
            human_tokens(app.ctx_used),
            human_tokens(app.ctx_window)
        )),
    ];
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn mode_label(m: crate::perms::Mode) -> &'static str {
    label_mode(m)
}

fn human_tokens(n: u64) -> String {
    fn one_decimal(v: f64) -> String {
        let s = format!("{v:.1}");
        if let Some(stripped) = s.strip_suffix(".0") {
            stripped.to_owned()
        } else {
            s
        }
    }
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{}k", one_decimal(n as f64 / 1000.0))
    } else {
        format!("{}M", one_decimal(n as f64 / 1_000_000.0))
    }
}

fn render_input(f: &mut Frame, area: ratatui::layout::Rect, app: &App, ir: InputRender) {
    let height = area.height as usize;
    let width = area.width as usize;

    if app.input.is_empty() && !app.running {
        let ph = format!("> {}", placeholder_text());
        let wrapped = wrap_segments(&[(ph, theme::dim())], width);
        let take: Vec<Line> = wrapped
            .into_iter()
            .take(height)
            .map(|row| {
                Line::from(
                    row.into_iter()
                        .map(|(t, s)| Span::styled(t, s))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        f.render_widget(Paragraph::new(take), area);
        return;
    }

    let shown_from = ir.cursor_row.saturating_sub(height.saturating_sub(1));
    let lines: Vec<Line> = ir
        .rows
        .iter()
        .skip(shown_from)
        .take(height)
        .map(|row| {
            Line::from(
                row.iter()
                    .map(|(t, s)| Span::styled(t.clone(), *s))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);

    let vis_row = ir.cursor_row - shown_from;
    if vis_row < height {
        let x = area.x as usize + ir.cursor_col;
        let y = area.y as usize + vis_row;
        if x < area.x as usize + width {
            f.set_cursor_position(ratatui::prelude::Position::new(x as u16, y as u16));
        }
    }
}

/// Visually wrapped input lines plus the terminal position of the cursor.
pub struct InputRender {
    rows: Vec<Vec<Seg>>,
    cursor_row: usize,
    cursor_col: usize,
}

pub fn build_input_rows(app: &App, width: usize) -> InputRender {
    let mut rows: Vec<Vec<Seg>> = vec![Vec::new()];
    let mut row_w = 0usize;
    let mut cursor: Option<(usize, usize)> = None;
    let mention_style = theme::blue_dim();

    // first line gets "> ", continuations get "  " - same as before
    let mut expect_prefix = true;
    for (gi, ch) in app.input.text.iter().enumerate() {
        if *ch == '\n' {
            if gi == app.input.cursor {
                cursor = Some((rows.len() - 1, row_w));
            }
            rows.push(Vec::new());
            row_w = 0;
            expect_prefix = true;
            continue;
        }
        if expect_prefix {
            let prefix = if rows.len() == 1 && rows[0].is_empty() {
                "> "
            } else {
                "  "
            };
            for pc in prefix.chars() {
                push_input_char(&mut rows, &mut row_w, pc, theme::normal(), width);
            }
            expect_prefix = false;
        }
        // snapshot AFTER the prefix: a cursor at the line start must sit on
        // the first text cell, not on top of the "> " marker
        if gi == app.input.cursor {
            cursor = Some((rows.len() - 1, row_w));
        }
        let st = if app.input.mentions.iter().any(|(a, b)| gi >= *a && gi < *b) {
            mention_style
        } else {
            theme::normal()
        };
        push_input_char(&mut rows, &mut row_w, *ch, st, width);
    }
    let (cursor_row, cursor_col) = cursor.unwrap_or((rows.len() - 1, row_w));

    // ghost completion hint trails after the typed text
    if let Some(tail) = app.input.ghost_tail() {
        for c in tail.chars() {
            push_input_char(&mut rows, &mut row_w, c, theme::dim(), width);
        }
    }

    InputRender {
        rows,
        cursor_row,
        cursor_col,
    }
}

fn push_input_char(rows: &mut Vec<Vec<Seg>>, row_w: &mut usize, c: char, st: Style, width: usize) {
    let cw = c.width().unwrap_or(0);
    if *row_w + cw > width && *row_w > 0 {
        rows.push(Vec::new());
        *row_w = 0;
    }
    let row = rows.last_mut().unwrap();
    match row.last_mut() {
        Some((t, s)) if *s == st => t.push(c),
        _ => row.push((c.to_string(), st)),
    }
    *row_w += cw;
}

fn placeholder_text() -> &'static str {
    "ask few anything · /commands · shift+enter newline"
}

fn render_palette(f: &mut Frame, area: ratatui::layout::Rect, items: &[String], sel: usize) {
    if area.height == 0 || items.is_empty() {
        return;
    }
    let max = area.height as usize;
    let sel = sel.min(items.len().saturating_sub(1));
    let start = sel.saturating_sub(max - 1);
    let window: Vec<Line> = items
        .iter()
        .enumerate()
        .skip(start)
        .take(max)
        .map(|(i, label)| {
            let style = if i == sel {
                theme::normal()
            } else {
                theme::dim()
            };
            let marker = if i == sel { "> " } else { "  " };
            Line::from(Span::styled(format!("{marker}{label}"), style))
        })
        .collect();
    f.render_widget(Paragraph::new(window), area);
}

pub fn current_palette(app: &App) -> Option<Vec<String>> {
    let items = palette_items(app)?;
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

fn palette_items(app: &App) -> Option<Vec<String>> {
    let text = app.input.text();
    if !text.starts_with('/') || text.contains('\n') {
        return None;
    }
    if text.contains(' ') {
        let cmd = find_command(&text)?;
        let after = text.split_once(' ').map(|x| x.1).unwrap_or("");
        let opts: Vec<String> = match cmd.arg_kind {
            crate::commands::ArgKind::Models => merge_models(app),
            kind => arg_options(kind, &[]),
        };
        let lowered = after.to_lowercase();
        let filtered: Vec<String> = opts
            .into_iter()
            .filter(|o| o.to_lowercase().starts_with(&lowered))
            .collect();
        return if filtered.is_empty() {
            None
        } else {
            Some(filtered)
        };
    }
    let typed = text.trim_start_matches('/');
    let names = filter_commands(typed);
    if names.is_empty() {
        None
    } else {
        Some(names.into_iter().map(|n| format!("/{n}")).collect())
    }
}

fn merge_models(app: &App) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in std::iter::once(app.model_name.clone()).chain(app.models_cache.iter().cloned()) {
        if !m.is_empty() && !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

fn focus_style(style: Style, focused: bool) -> Style {
    if focused {
        theme::normal()
    } else {
        style
    }
}

fn build_rows(app: &App, width: usize) -> Vec<(Vec<Seg>, Hit)> {
    let mut rows: Vec<(Vec<Seg>, Hit)> = Vec::new();
    let cap = app.cfg.diff_lines.max(10);

    for (bi, block) in app.blocks.iter().enumerate() {
        // A step group with no actual step (only interim thought/narration) has
        // nothing meaningful to headline - skip it so "0 steps" never shows.
        if matches!(block, Block::Steps(g) if !g.steps.iter().any(|s| s.is_step())) {
            continue;
        }
        if !rows.is_empty() {
            rows.push((vec![], Hit::Nothing));
        }
        match block {
            Block::User(text) => {
                // the `>` quote is flush-left; wrapped continuation lines hang-
                // indent under the text after `> ` to keep one coherent block
                push_user_prompt(&mut rows, text, theme::normal(), width);
            }
            Block::Assistant(text) => {
                push_markdown_text(&mut rows, text, width, Hit::Nothing, 0);
            }
            Block::Resumed(resumed) => render_resumed(
                &mut rows,
                resumed,
                bi,
                width,
                app.focus == Some((bi, usize::MAX)),
            ),
            Block::Steps(group) => render_steps(&mut rows, group, bi, width, cap, app.focus),
            Block::Notice { text, level } => {
                let style = match level {
                    NoticeLevel::Info => theme::dim(),
                    NoticeLevel::Error => theme::red(),
                    NoticeLevel::Warn => theme::amber(),
                };
                push_wrapped_text(&mut rows, text, style, width, Hit::Nothing);
            }
            Block::Remembered(text) => {
                push_wrapped_text(
                    &mut rows,
                    &format!("remembered: {text}"),
                    theme::blue_dim(),
                    width,
                    Hit::Nothing,
                );
            }
            Block::MemoryView { text } => {
                for l in text.lines() {
                    let style = if l.starts_with(' ') {
                        theme::dim()
                    } else {
                        theme::blue_dim()
                    };
                    push_wrapped_text(&mut rows, l, style, width, Hit::Nothing);
                }
            }
            Block::PermAsk(ask) => render_permission(&mut rows, ask, bi, width),
        }
    }

    // live action currently executing, in present tense; replaced by the final
    // past-tense step once it completes. The reasoning timer is shown in the
    // status strip above the input, so we do not duplicate it here.
    if let Some((doing, arg)) = &app.live_step {
        rows.push((vec![], Hit::Nothing));
        push_wrapped_text(
            &mut rows,
            &format!("  {doing} {arg}"),
            theme::dim(),
            width,
            Hit::Nothing,
        );
    }

    rows
}

fn render_resumed(
    rows: &mut Vec<(Vec<Seg>, Hit)>,
    resumed: &ResumedSession,
    block_idx: usize,
    width: usize,
    focused: bool,
) {
    let marker = if resumed.expanded { 'v' } else { '>' };
    push_wrapped_text(
        rows,
        &format!("{marker} {}", resumed.label),
        focus_style(theme::dim(), focused),
        width,
        Hit::Block(block_idx),
    );
    if !resumed.expanded {
        return;
    }
    for item in &resumed.items {
        match item {
            ResumedItem::User(text) => {
                push_nested_user_prompt(rows, text, theme::normal(), width, 2)
            }
            ResumedItem::Assistant(text) => push_markdown_text(rows, text, width, Hit::Nothing, 2),
            ResumedItem::Step(text) => push_indented(rows, text, theme::dim(), width, 2),
        }
    }
}

fn render_steps(
    rows: &mut Vec<(Vec<Seg>, Hit)>,
    group: &StepsGroup,
    block_idx: usize,
    width: usize,
    cap: usize,
    focus: Option<(usize, usize)>,
) {
    let marker = if group.expanded { 'v' } else { '>' };
    push_wrapped_text(
        rows,
        &format!("{marker} {}", group.summary()),
        focus_style(theme::dim(), focus == Some((block_idx, usize::MAX))),
        width,
        Hit::Step(block_idx, usize::MAX),
    );
    if !group.expanded {
        return;
    }
    for (step_idx, item) in group.steps.iter().enumerate() {
        render_step_item(rows, item, block_idx, step_idx, width, cap, focus);
    }
}

fn render_step_item(
    rows: &mut Vec<(Vec<Seg>, Hit)>,
    item: &StepItem,
    block_idx: usize,
    step_idx: usize,
    width: usize,
    cap: usize,
    focus: Option<(usize, usize)>,
) {
    let hit = Hit::Step(block_idx, step_idx);
    let focused = focus == Some((block_idx, step_idx));
    match item {
        StepItem::Step(step) => {
            let style = match step.view.verb.word() {
                "failed" | "error" => theme::red(),
                "denied" => theme::amber(),
                _ => theme::dim(),
            };
            push_indented_hit(
                rows,
                &step.headline(),
                focus_style(style, focused),
                width,
                2,
                hit,
            );
            // Detail::Message (permission-denial reasons, tool error text) is
            // always visible: such steps are not navigable, so a collapsed
            // state could never be toggled and the text would be lost.
            let always_inline = matches!(step.view.detail, Some(crate::agent::Detail::Message(_)));
            if step.expand == Expand::Collapsed && !always_inline {
                return;
            }
            for detail in step.detail_rows(cap) {
                let style = if detail.starts_with('+') {
                    theme::green()
                } else if detail.starts_with('-') {
                    theme::red()
                } else {
                    theme::dim()
                };
                push_indented_hit(rows, &detail, style, width, 4, hit);
            }
        }
        StepItem::Thought { text, expand } => {
            render_folded_text(rows, "thought", text, *expand, width, hit, focused)
        }
        StepItem::Narration { text, expand } => {
            render_folded_text(rows, "said", text, *expand, width, hit, focused)
        }
    }
}

fn render_folded_text(
    rows: &mut Vec<(Vec<Seg>, Hit)>,
    label: &str,
    text: &str,
    expand: Expand,
    width: usize,
    hit: Hit,
    focused: bool,
) {
    let marker = if expand == Expand::Collapsed {
        '>'
    } else {
        'v'
    };
    push_wrapped_text(
        rows,
        &format!("  {marker} {label}"),
        focus_style(theme::dim(), focused),
        width,
        hit,
    );
    if expand != Expand::Collapsed {
        push_indented(rows, text, theme::dim(), width, 4);
    }
}

fn render_permission(
    rows: &mut Vec<(Vec<Seg>, Hit)>,
    ask: &PermAskBlock,
    block_idx: usize,
    width: usize,
) {
    if let Some(choice) = ask.resolved {
        let text = format!("{} {} ({choice})", ask.verb, ask.target);
        push_wrapped_text(rows, &text, theme::amber_dim(), width, Hit::Nothing);
        return;
    }
    let header = format!(
        "? {} {}{}  [{}]",
        ask.verb,
        ask.target,
        if ask.sensitive { " (sensitive)" } else { "" },
        ask.cap_label
    );
    push_wrapped_text(rows, &header, theme::amber(), width, Hit::Nothing);
    for (option_idx, option) in PERM_OPTIONS.iter().enumerate() {
        let style = if option_idx == ask.selected {
            theme::amber()
        } else {
            theme::normal()
        };
        rows.push((
            vec![(format!("  {} {option}", option_idx + 1), style)],
            Hit::PermOption(block_idx, option_idx),
        ));
    }
}

fn push_wrapped_text(
    rows: &mut Vec<(Vec<Seg>, Hit)>,
    text: &str,
    style: Style,
    width: usize,
    hit: Hit,
) {
    for segs in wrap_segments(&[(text.to_owned(), style)], width) {
        rows.push((segs, hit));
    }
}

/// Render model prose with the small Markdown subset understood by Few.
/// Prose wraps by words; code-fence lines preserve their whitespace and wrap
/// only at terminal columns. Code gets a two-space nesting indent so source
/// remains legible without a panel or background.
fn push_markdown_text(
    rows: &mut Vec<(Vec<Seg>, Hit)>,
    text: &str,
    width: usize,
    hit: Hit,
    indent: usize,
) {
    for line in markdown::parse(text) {
        match line {
            MarkdownLine::Blank => rows.push((vec![], Hit::Nothing)),
            MarkdownLine::Prose { prefix, content } => {
                let inner = width.saturating_sub(indent).max(1);
                let prefix_width = segments_width(&prefix);
                let content_width = inner.saturating_sub(prefix_width).max(1);
                for (i, segs) in wrap_markdown_words(&content, content_width)
                    .into_iter()
                    .enumerate()
                {
                    let mut row = Vec::new();
                    push_seg(&mut row, " ".repeat(indent), theme::normal());
                    if i == 0 {
                        for (text, style) in &prefix {
                            push_seg(&mut row, text.clone(), *style);
                        }
                    } else {
                        push_seg(&mut row, " ".repeat(prefix_width), theme::normal());
                    }
                    row.extend(segs);
                    rows.push((row, hit));
                }
            }
            MarkdownLine::Code(segments) => {
                let code_indent = indent + 2;
                let inner = width.saturating_sub(code_indent).max(1);
                for segs in wrap_markdown_code(&segments, inner) {
                    let mut row = Vec::new();
                    push_seg(&mut row, " ".repeat(code_indent), theme::dim());
                    row.extend(segs);
                    rows.push((row, hit));
                }
            }
        }
    }
}

fn wrap_markdown_words(segs: &[Seg], width: usize) -> Vec<Vec<Seg>> {
    struct Word {
        space_before: bool,
        segments: Vec<Seg>,
    }

    let mut words = Vec::new();
    let mut current: Vec<Seg> = Vec::new();
    let mut pending_space = false;
    let mut current_space = false;
    for (text, style) in segs {
        for ch in text.chars() {
            if ch.is_whitespace() {
                if !current.is_empty() {
                    words.push(Word {
                        space_before: current_space,
                        segments: std::mem::take(&mut current),
                    });
                }
                pending_space = true;
            } else {
                if current.is_empty() {
                    current_space = pending_space;
                    pending_space = false;
                }
                push_seg(&mut current, ch.to_string(), *style);
            }
        }
    }
    if !current.is_empty() {
        words.push(Word {
            space_before: current_space,
            segments: current,
        });
    }

    let width = width.max(1);
    let mut rows: Vec<Vec<Seg>> = vec![Vec::new()];
    let mut row_width = 0usize;
    for word in words {
        let word_width = segments_width(&word.segments);
        let separator = usize::from(word.space_before && row_width > 0);
        if row_width > 0 && row_width + separator + word_width > width {
            rows.push(Vec::new());
            row_width = 0;
        }
        if word.space_before && row_width > 0 {
            push_seg(rows.last_mut().unwrap(), " ".into(), theme::normal());
            row_width += 1;
        }
        for (text, style) in word.segments {
            for ch in text.chars() {
                let char_width = ch.width().unwrap_or(0);
                if row_width > 0 && row_width + char_width > width {
                    rows.push(Vec::new());
                    row_width = 0;
                }
                push_seg(rows.last_mut().unwrap(), ch.to_string(), style);
                row_width += char_width;
            }
        }
    }
    rows
}

fn wrap_markdown_code(segs: &[Seg], width: usize) -> Vec<Vec<Seg>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Seg>> = vec![Vec::new()];
    let mut row_width = 0usize;
    for (text, style) in segs {
        for ch in text.chars() {
            let char_width = ch.width().unwrap_or(0);
            if row_width > 0 && row_width + char_width > width {
                rows.push(Vec::new());
                row_width = 0;
            }
            push_seg(rows.last_mut().unwrap(), ch.to_string(), *style);
            row_width += char_width;
        }
    }
    rows
}

fn segments_width(segs: &[Seg]) -> usize {
    segs.iter()
        .flat_map(|(text, _)| text.chars())
        .map(|ch| ch.width().unwrap_or(0))
        .sum()
}

fn push_seg(row: &mut Vec<Seg>, text: String, style: Style) {
    if text.is_empty() {
        return;
    }
    match row.last_mut() {
        Some((last, previous)) if *previous == style => last.push_str(&text),
        _ => row.push((text, style)),
    }
}

fn push_indented(
    rows: &mut Vec<(Vec<Seg>, Hit)>,
    text: &str,
    style: Style,
    width: usize,
    indent: usize,
) {
    // Wrap inside the indented width and re-apply the indent on every wrapped
    // row, so continued lines keep the same depth as the first.
    let inner = width.saturating_sub(indent).max(1);
    let pad = " ".repeat(indent);
    for l in text.lines() {
        for segs in wrap_segments(&[(l.to_owned(), style)], inner) {
            let mut row: Vec<Seg> = vec![(pad.clone(), style)];
            row.extend(segs);
            rows.push((row, Hit::Nothing));
        }
    }
}

/// Like `push_indented` but attaches a click hit to every produced row.
fn push_indented_hit(
    rows: &mut Vec<(Vec<Seg>, Hit)>,
    text: &str,
    style: Style,
    width: usize,
    indent: usize,
    hit: Hit,
) {
    let inner = width.saturating_sub(indent).max(1);
    let pad = " ".repeat(indent);
    for l in text.lines() {
        for segs in wrap_segments(&[(l.to_owned(), style)], inner) {
            let mut row: Vec<Seg> = vec![(pad.clone(), style)];
            row.extend(segs);
            rows.push((row, hit));
        }
    }
}

/// Render the user's submitted prompt as a coherent quoted block: the `>`
/// quote sits flush-left (no left margin), and continuation lines are
/// hang-indented by 2 chars so they align under the text after `> `.
fn push_user_prompt(rows: &mut Vec<(Vec<Seg>, Hit)>, text: &str, style: Style, width: usize) {
    let indent = 2usize; // "> " on the first line, "  " on continuation
    let inner = width.saturating_sub(indent).max(1);
    let wrapped = wrap_segments(&[(text.to_owned(), style)], inner);
    for (i, segs) in wrapped.iter().enumerate() {
        let pad: &str = if i == 0 { "> " } else { "  " };
        let mut row: Vec<Seg> = vec![(pad.to_string(), style)];
        row.extend(segs.iter().cloned());
        rows.push((row, Hit::Nothing));
    }
}

fn push_nested_user_prompt(
    rows: &mut Vec<(Vec<Seg>, Hit)>,
    text: &str,
    style: Style,
    width: usize,
    indent: usize,
) {
    let prefix = indent + 2;
    let inner = width.saturating_sub(prefix).max(1);
    let wrapped = wrap_segments(&[(text.to_owned(), style)], inner);
    for (i, segs) in wrapped.iter().enumerate() {
        let mut row = vec![(
            format!("{}{}", " ".repeat(indent), if i == 0 { "> " } else { "  " }),
            style,
        )];
        row.extend(segs.iter().cloned());
        rows.push((row, Hit::Nothing));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::agent::{StepView, Verb};
    use crate::app::App;
    use crate::config::Config;
    use crate::memory::Memory;
    use crate::perms::{Mode, PermEngine, Policy};
    use crate::providers::openai::OpenAiProvider;
    use crate::transcript::{
        PermAskBlock, ResumedItem, ResumedSession, StepBlock, StepItem, StepsGroup,
    };
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    fn test_app(tag: &str) -> App {
        let root = std::env::temp_dir().join(format!("few-ui-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cfg = Arc::new(Config {
            model: "test-model".into(),
            context_window: 200_000,
            project_root: root.clone(),
            project_config_path: root.join(".few/config.toml"),
            project_detected: true,
            ..Default::default()
        });
        let perms = Arc::new(Mutex::new(PermEngine::new(
            root.clone(),
            vec![],
            Default::default(),
            Policy::Ask,
            Policy::Ask,
            true,
        )));
        let memory = Memory::new(&root, &root.join(".data"));
        let provider = OpenAiProvider::new("http://127.0.0.1:9/v1", None, "test-model").unwrap();
        let agent = Arc::new(Agent::new(
            provider,
            cfg.clone(),
            perms,
            memory.clone(),
            Default::default(),
        ));
        let mut app = App::new(
            cfg,
            agent,
            memory,
            root.join("hist.txt"),
            root.join("sessions"),
            None,
            Vec::new(),
        );
        app.model_name = "sonnet-5".into();
        app.mode = Mode::Build;
        app.ctx_used = 12_400;
        app
    }

    fn render(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer();
        let mut rows = Vec::new();
        for y in 0..h {
            let mut line = String::new();
            for x in 0..w {
                line += buf[(x, y)].symbol();
            }
            rows.push(line.trim_end().to_owned());
        }
        rows
    }

    #[test]
    fn status_bar_and_placeholder_contract() {
        let mut app = test_app("status");
        let rows = render(&mut app, 80, 10);
        let status = &rows[9];
        assert!(status.contains("model:"), "{status:?}");
        assert!(status.contains("sonnet-5"), "{status:?}");
        assert!(status.contains("mode:"), "{status:?}");
        assert!(status.contains("build"), "{status:?}");
        assert!(status.contains("ctx:"), "{status:?}");
        assert!(status.contains("12.4k / 200k (6%)"), "{status:?}");
        let sep = char::from_u32(0x2500).unwrap().to_string().repeat(80);
        assert_eq!(rows[8], sep, "thin separator above status bar");
        let joined = rows.join("\n");
        assert!(joined.contains("ask few anything"), "{joined:?}");
    }

    #[test]
    fn assistant_markdown_preserves_structure_and_highlights_code() {
        let mut app = test_app("assistant-markdown");
        app.blocks.push(Block::Assistant(
            "# Result\n\nUse `cargo test`, then **inspect** it.\n\n- first item with enough text to wrap cleanly\n- second item\n\n> quoted note\n\n```rust\nfn main() {\n    let count = 42;\n    println!(\"done\"); // status\n}\n```"
                .into(),
        ));

        let built = build_rows(&app, 44);
        let joined = built
            .iter()
            .map(|(row, _)| {
                row.iter()
                    .map(|(text, _)| text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Result"), "{joined:?}");
        assert!(
            joined.contains("Use cargo test, then inspect it."),
            "{joined:?}"
        );
        assert!(joined.contains("- first item"), "{joined:?}");
        assert!(joined.contains("> quoted note"), "{joined:?}");
        assert!(joined.contains("  fn main()"), "{joined:?}");
        assert!(!joined.contains("```"), "fence markers must not render");
        assert!(!joined.contains("**"), "emphasis markers must not render");

        let segments: Vec<&Seg> = built.iter().flat_map(|(row, _)| row.iter()).collect();
        assert!(segments.iter().any(|(text, style)| {
            text == "Result" && style.add_modifier.contains(ratatui::style::Modifier::BOLD)
        }));
        for inline_word in ["cargo", "test"] {
            assert!(segments.iter().any(|(text, style)| {
                text == inline_word && style.fg == Some(ratatui::style::Color::Blue)
            }));
        }
        assert!(segments.iter().any(|(text, style)| {
            text == "fn"
                && style.fg == Some(ratatui::style::Color::Blue)
                && style.add_modifier.contains(ratatui::style::Modifier::BOLD)
        }));
        assert!(segments.iter().any(|(text, style)| {
            text.contains("// status") && style.add_modifier.contains(ratatui::style::Modifier::DIM)
        }));

        insta::assert_snapshot!("assistant_markdown", render(&mut app, 60, 24).join("\n"));
    }

    #[test]
    fn collapsed_steps_summary_with_error_count() {
        let mut app = test_app("steps");
        app.blocks.push(Block::Steps(StepsGroup {
            steps: vec![
                StepItem::Step(StepBlock {
                    view: crate::agent::StepView {
                        verb: crate::agent::Verb::Read,
                        arg: "README.md".into(),
                        detail: None,
                    },
                    expand: Expand::Collapsed,
                }),
                StepItem::Step(StepBlock {
                    view: crate::agent::StepView {
                        verb: crate::agent::Verb::Failed,
                        arg: "cargo test".into(),
                        detail: None,
                    },
                    expand: Expand::Collapsed,
                }),
                StepItem::Step(StepBlock {
                    view: crate::agent::StepView {
                        verb: crate::agent::Verb::Ran,
                        arg: "cargo build".into(),
                        detail: None,
                    },
                    expand: Expand::Collapsed,
                }),
            ],
            expanded: false,
            outcome: Some(crate::agent::TaskOutcome::Done),
        }));
        let rows = render(&mut app, 60, 12);
        let joined = rows.join("\n");
        assert!(joined.contains("> 3 steps · 1 error"), "{joined:?}");
        assert!(
            !joined.contains("README.md"),
            "collapsed group must hide step args"
        );
    }

    #[test]
    fn message_detail_renders_despite_collapsed_state() {
        let mut app = test_app("msgdetail");
        app.blocks.push(Block::Steps(StepsGroup {
            steps: vec![StepItem::Step(StepBlock {
                view: crate::agent::StepView {
                    verb: crate::agent::Verb::Denied,
                    arg: "write src/main.rs".into(),
                    detail: Some(crate::agent::Detail::Message(
                        "write denied in plan mode".into(),
                    )),
                },
                // Message steps are created collapsed and are not navigable,
                // so their text must render regardless of expand state.
                expand: Expand::Collapsed,
            })],
            expanded: true,
            outcome: None,
        }));
        let rows = render(&mut app, 60, 12);
        let joined = rows.join("\n");
        assert!(
            joined.contains("write denied in plan mode"),
            "Message detail must be visible while collapsed: {joined:?}"
        );
    }

    fn step_with_diff(arg: &str) -> StepBlock {
        StepBlock {
            view: crate::agent::StepView {
                verb: crate::agent::Verb::Wrote,
                arg: arg.into(),
                detail: Some(crate::agent::Detail::Diff {
                    lines: vec![crate::diffgen::DiffLine {
                        sign: '+',
                        text: "new line".into(),
                    }],
                    capped_at: None,
                }),
            },
            expand: Expand::Collapsed,
        }
    }

    #[test]
    fn expandable_targets_in_visual_order() {
        let mut app = test_app("targets");
        // one turn group: a thought, a diff-capable write step, and a ran
        // step whose Message detail does not respect Expand
        app.blocks.push(Block::Steps(StepsGroup {
            steps: vec![
                StepItem::Thought {
                    text: "considering options".into(),
                    expand: Expand::Collapsed,
                },
                StepItem::Step(step_with_diff("a.txt")),
                StepItem::Step(StepBlock {
                    view: crate::agent::StepView {
                        verb: crate::agent::Verb::Ran,
                        arg: "cargo fmt".into(),
                        detail: Some(crate::agent::Detail::Message("ok".into())),
                    },
                    expand: Expand::Collapsed,
                }),
            ],
            expanded: false,
            outcome: None,
        }));

        // group header, thought item, diff-capable step only
        // (Message detail does not respect Expand, so it is not a target)
        assert_eq!(
            app.expandable_targets(),
            vec![(0, usize::MAX), (0, 0), (0, 1)]
        );
    }

    #[test]
    fn focus_moves_wraps_and_space_toggles() {
        let mut app = test_app("focus");
        app.blocks.push(Block::Steps(StepsGroup {
            steps: vec![
                StepItem::Thought {
                    text: "considering options".into(),
                    expand: Expand::Collapsed,
                },
                StepItem::Step(step_with_diff("a.txt")),
            ],
            expanded: false,
            outcome: None,
        }));

        app.move_focus(true);
        assert_eq!(app.focus, Some((0, usize::MAX)));
        app.move_focus(false);
        assert_eq!(app.focus, Some((0, 1)), "wraps backwards to last target");

        app.focus = Some((0, usize::MAX));
        assert!(app.toggle_focused());
        match &app.blocks[0] {
            Block::Steps(g) => assert!(g.expanded, "space on group headline expands it"),
            _ => panic!("expected steps block"),
        }

        app.focus = Some((0, 1));
        assert!(app.toggle_focused());
        match &app.blocks[0] {
            Block::Steps(g) => match &g.steps[1] {
                StepItem::Step(s) => assert_eq!(s.expand, Expand::Shown),
                _ => panic!("expected step item"),
            },
            _ => panic!("expected steps block"),
        }
    }

    #[test]
    fn live_step_renders_in_present_tense() {
        let mut app = test_app("live-step");
        app.blocks.push(Block::Steps(StepsGroup {
            steps: vec![],
            expanded: false,
            outcome: None,
        }));
        app.live_step = Some(("reading".into(), "src/main.rs".into()));
        let rows = render(&mut app, 60, 12);
        let joined = rows.join("\n");
        assert!(joined.contains("reading src/main.rs"), "{joined:?}");

        // once the step completes, only the past-tense form remains
        app.live_step = None;
        let rows = render(&mut app, 60, 12);
        assert!(!rows.join("\n").contains("reading"));
    }

    #[test]
    fn no_spinner_in_busy_line() {
        let mut app = test_app("busy");
        app.running = true;
        app.started_at = Some(Instant::now());
        let rows = render(&mut app, 60, 12);
        let joined = rows.join("\n");
        assert!(joined.contains("working · "), "{joined:?}");
        for frame in ['|', '/', '-', '\\'] {
            assert!(
                !joined.contains(&format!("{frame} working")),
                "spinner frames are gone"
            );
        }
    }

    #[test]
    fn cursor_at_line_start_sits_after_the_prompt_marker() {
        let mut app = test_app("input-cursor-home");
        app.input.set_text("abc");
        app.input.home();
        let ir = build_input_rows(&app, 40);
        // "> " occupies columns 0-1; a cursor on the first character must
        // land on column 2, not on top of the marker
        assert_eq!(ir.cursor_row, 0);
        assert_eq!(ir.cursor_col, 2, "cursor must not overlap the '> ' prefix");

        // continuation lines are indented by two spaces, same rule
        app.input.set_text("a\nb");
        app.input.cursor = 2;
        let ir2 = build_input_rows(&app, 40);
        assert_eq!(ir2.cursor_row, 1);
        assert_eq!(ir2.cursor_col, 2, "cursor must not overlap the indent");

        // an empty input still reports a position after the marker
        app.input.set_text("");
        let ir3 = build_input_rows(&app, 40);
        assert_eq!(ir3.cursor_row, 0);
    }

    #[test]
    fn long_input_wraps_and_cursor_follows() {
        let mut app = test_app("input-wrap");
        let long = "a".repeat(100);
        app.input.set_text(&long);
        let ir = build_input_rows(&app, 40);
        assert!(
            ir.rows.len() >= 3,
            "long line must wrap, got {}",
            ir.rows.len()
        );
        // cursor sits at the end of the text
        assert!(ir.cursor_row >= 2);

        // CJK characters count double and are not split
        app.input.set_text("日本語テスト");
        let ir2 = build_input_rows(&app, 8);
        assert!(ir2.rows.len() >= 2, "cjk line must wrap by display width");
    }

    #[test]
    fn wide_content_is_wrapped_not_clipped() {
        let mut app = test_app("wide-detail");
        let long_out = format!("out-{}", "x".repeat(120));
        app.blocks.push(Block::Steps(StepsGroup {
            steps: vec![StepItem::Step(StepBlock {
                view: crate::agent::StepView {
                    verb: crate::agent::Verb::Ran,
                    arg: "cmd".into(),
                    detail: Some(crate::agent::Detail::Output {
                        text: long_out,
                        total_bytes: 124,
                        truncated: false,
                    }),
                },
                expand: Expand::Shown,
            })],
            expanded: true,
            outcome: None,
        }));
        let rows = render(&mut app, 50, 20);
        let joined = rows.join("\n");
        // the tail of the long output must survive wrapping (not be clipped)
        let tail = &"x".repeat(120)[100..];
        assert!(joined.contains(tail), "wrapped output must keep its tail");
    }

    #[test]
    fn focus_is_contrast_not_reverse() {
        let mut app = test_app("focus-contrast");
        app.blocks.push(Block::Steps(StepsGroup {
            // a group needs at least one step to be rendered; the test only
            // cares about the header contrast, so a single write step suffices
            steps: vec![StepItem::Step(StepBlock {
                view: StepView {
                    verb: Verb::Wrote,
                    arg: "x".into(),
                    detail: None,
                },
                expand: Expand::Collapsed,
            })],
            expanded: false,
            outcome: None,
        }));

        let headline_style = |app: &App| -> Style {
            build_rows(app, 40)
                .iter()
                .find(|(_, hit)| matches!(hit, Hit::Step(_, usize::MAX)))
                .and_then(|(segs, _)| segs.first())
                .map(|(_, s)| *s)
                .unwrap()
        };

        app.focus = None;
        assert_eq!(headline_style(&app), theme::dim());
        // the cursor is pure contrast: focused dim text renders normal,
        // no reverse video, no extra glyph
        app.focus = Some((0, usize::MAX));
        assert_eq!(headline_style(&app), theme::normal());
    }

    #[test]
    fn wrapped_step_keeps_indent() {
        let mut app = test_app("wrap-indent");
        app.blocks.push(Block::Steps(StepsGroup {
            steps: vec![StepItem::Step(StepBlock {
                view: StepView {
                    verb: Verb::Ran,
                    arg: "cd /tmp/few-test-project && python3 -m py_compile greet.py && echo OK"
                        .into(),
                    detail: None,
                },
                expand: Expand::Collapsed,
            })],
            expanded: true,
            outcome: None,
        }));
        // only the step item rows (si != MAX, which is the group header)
        let step_rows: Vec<String> = build_rows(&app, 40)
            .into_iter()
            .filter(|(_, h)| matches!(h, Hit::Step(_, si) if *si != usize::MAX))
            .map(|(segs, _)| segs.into_iter().map(|(t, _)| t).collect())
            .collect();
        assert!(
            step_rows.len() >= 2,
            "expected the step to wrap across rows, got {step_rows:?}"
        );
        for r in &step_rows {
            assert!(
                r.starts_with("  "),
                "wrapped step line lost its 2-space depth: {r:?}"
            );
        }
    }

    #[test]
    fn no_zero_steps_group_when_only_thought() {
        let mut app = test_app("no-zero");
        app.blocks.push(Block::Steps(StepsGroup {
            steps: vec![StepItem::Thought {
                text: "interim thinking".into(),
                expand: Expand::Collapsed,
            }],
            expanded: false,
            outcome: None,
        }));
        // a group with no actual step must not render at all, so the
        // "0 steps" header never appears
        assert!(
            build_rows(&app, 40).is_empty(),
            "a 0-step group should be skipped entirely"
        );
    }

    #[test]
    fn resolved_ask_is_dimmed_amber_without_glyph() {
        let mut app = test_app("resolved");
        app.blocks.push(Block::PermAsk(PermAskBlock {
            id: 1,
            verb: "write".into(),
            target: ".env".into(),
            cap_label: "filesystem.write".into(),
            sensitive: true,
            selected: 0,
            resolved: Some("allow once"),
        }));
        let rows = render(&mut app, 60, 12);
        let joined = rows.join("\n");
        assert!(joined.contains("write .env (allow once)"), "{joined:?}");
        assert!(!joined.contains('✓'), "no unicode glyphs in resolved line");
    }

    #[test]
    fn selected_permission_option_uses_color_without_marker() {
        let mut app = test_app("permission-marker");
        app.blocks.push(Block::PermAsk(PermAskBlock {
            id: 1,
            verb: "write".into(),
            target: "src/main.rs".into(),
            cap_label: "filesystem.write".into(),
            sensitive: false,
            selected: 1,
            resolved: None,
        }));

        let option_rows: Vec<_> = build_rows(&app, 80)
            .into_iter()
            .filter(|(_, hit)| matches!(hit, Hit::PermOption(_, _)))
            .collect();
        assert_eq!(option_rows[1].0[0].0, "  2 allow for this session");
        assert_eq!(option_rows[1].0[0].1, theme::amber());
    }

    #[test]
    fn mouse_click_on_thought_toggles() {
        let mut app = test_app("thought-click");
        // expanded group so the folded thought item is rendered and clickable
        app.blocks.push(Block::Steps(StepsGroup {
            steps: vec![
                StepItem::Thought {
                    text: "considering options".into(),
                    expand: Expand::Collapsed,
                },
                StepItem::Step(step_with_diff("a.txt")),
            ],
            expanded: true,
            outcome: None,
        }));
        render(&mut app, 60, 12); // builds the hitmap
                                  // the thought is the first item inside the group (after the header)
        let row = app
            .hitmap
            .iter()
            .position(|hit| *hit == Hit::Step(0, 0))
            .expect("thought hit present");
        app.on_click(row as u16); // click the thought header wherever it landed
        match &app.blocks[0] {
            Block::Steps(g) => match &g.steps[0] {
                StepItem::Thought { expand, .. } => {
                    assert!(*expand != Expand::Collapsed, "click must unfold thought");
                }
                _ => panic!("expected thought item"),
            },
            _ => panic!("expected steps block"),
        }
        app.on_click(row as u16);
        match &app.blocks[0] {
            Block::Steps(g) => match &g.steps[0] {
                StepItem::Thought { expand, .. } => {
                    assert_eq!(*expand, Expand::Collapsed, "second click must fold thought");
                }
                _ => panic!("expected thought item"),
            },
            _ => panic!("expected steps block"),
        }
    }

    #[test]
    fn short_diff_collapses_on_second_click() {
        let mut app = test_app("diff-click");
        app.blocks.push(Block::Steps(StepsGroup {
            steps: vec![StepItem::Step(step_with_diff("a.txt"))],
            expanded: true,
            outcome: None,
        }));
        render(&mut app, 60, 12);
        let row = app
            .hitmap
            .iter()
            .position(|hit| *hit == Hit::Step(0, 0))
            .expect("diff step hit present");

        app.on_click(row as u16);
        app.on_click(row as u16);
        match &app.blocks[0] {
            Block::Steps(g) => match &g.steps[0] {
                StepItem::Step(step) => assert_eq!(step.expand, Expand::Collapsed),
                _ => panic!("expected step item"),
            },
            _ => panic!("expected steps block"),
        }
    }

    #[test]
    fn truncated_diff_reveals_full_before_collapsing() {
        let mut app = test_app("long-diff-click");
        let mut step = step_with_diff("a.txt");
        if let Some(crate::agent::Detail::Diff { lines, capped_at }) = &mut step.view.detail {
            lines.push(crate::diffgen::DiffLine {
                sign: '+',
                text: "hidden line".into(),
            });
            *capped_at = Some(1);
        }
        app.blocks.push(Block::Steps(StepsGroup {
            steps: vec![StepItem::Step(step)],
            expanded: true,
            outcome: None,
        }));
        render(&mut app, 60, 12);
        let row = app
            .hitmap
            .iter()
            .position(|hit| *hit == Hit::Step(0, 0))
            .unwrap();

        app.on_click(row as u16);
        app.on_click(row as u16);
        match &app.blocks[0] {
            Block::Steps(g) => match &g.steps[0] {
                StepItem::Step(step) => assert_eq!(step.expand, Expand::Full),
                _ => panic!("expected step item"),
            },
            _ => panic!("expected steps block"),
        }
        app.on_click(row as u16);
        match &app.blocks[0] {
            Block::Steps(g) => match &g.steps[0] {
                StepItem::Step(step) => assert_eq!(step.expand, Expand::Collapsed),
                _ => panic!("expected step item"),
            },
            _ => panic!("expected steps block"),
        }
    }

    #[test]
    fn resumed_session_is_collapsed_then_reveals_history() {
        let mut app = test_app("resume-block");
        app.blocks.push(Block::Resumed(ResumedSession {
            label: "resumed session · 4 messages restored".into(),
            items: vec![
                ResumedItem::User("Create hello.py".into()),
                ResumedItem::Step("wrote hello.py".into()),
                ResumedItem::Assistant("Done.".into()),
            ],
            expanded: false,
        }));

        let collapsed = build_rows(&app, 60)
            .into_iter()
            .map(|(row, _)| row.into_iter().map(|(s, _)| s).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(collapsed.contains("> resumed session · 4 messages restored"));
        assert!(!collapsed.contains("Create hello.py"));

        render(&mut app, 60, 14);
        let row = app
            .hitmap
            .iter()
            .position(|hit| *hit == Hit::Block(0))
            .unwrap();
        app.on_click(row as u16);
        let expanded = build_rows(&app, 60)
            .into_iter()
            .map(|(row, _)| row.into_iter().map(|(s, _)| s).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(expanded.contains("v resumed session · 4 messages restored"));
        assert!(expanded.contains("  > Create hello.py"));
        assert!(expanded.contains("  wrote hello.py"));
        assert!(expanded.contains("  Done."));
        insta::assert_snapshot!(
            "resumed_session_expanded",
            render(&mut app, 60, 14).join("\n")
        );
    }

    #[test]
    fn remembered_stays_visible_when_steps_are_collapsed() {
        let mut app = test_app("remembered");
        app.blocks.push(Block::Steps(StepsGroup {
            steps: vec![StepItem::Step(step_with_diff("a.txt"))],
            expanded: false,
            outcome: Some(crate::agent::TaskOutcome::Done),
        }));
        app.blocks.push(Block::Remembered("uses Fish".into()));

        let joined = render(&mut app, 60, 12).join("\n");
        assert!(joined.contains("> 1 steps"), "{joined:?}");
        assert!(joined.contains("remembered: uses Fish"), "{joined:?}");
    }
}

//! Drawing: the chat list, the bubble transcript, the composer and the
//! popups. Reads `App`, writes back only the viewport geometry the key
//! handling needs.

use imessage_common::model::{DeliveryState, Message, MessagePart, Service, link_host};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Focus, Mode};
use crate::view::{self, att_icon, first_name, full_time, human_size, short_clock, short_time};

const BLUE: Color = Color::Rgb(0, 122, 255);
const GREEN: Color = Color::Rgb(52, 199, 89);
/// Text colours used inside green (SMS/RCS) bubbles.
const GREEN_FG: Color = Color::Rgb(0, 25, 8);
const GREEN_MUTED: Color = Color::Rgb(20, 70, 35);
const GRAY: Color = Color::Rgb(58, 58, 60);
const DIM: Color = Color::Rgb(142, 142, 147);
const ACCENT: Color = Color::Rgb(255, 214, 10);
const HILITE: Color = Color::Rgb(44, 44, 46);

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(sidebar_width(area.width)), Constraint::Min(30)])
        .split(area);
    draw_sidebar(f, app, cols[0]);

    let compose_h = (app.compose.lines().len() as u16).clamp(1, 6) + 2;
    let chips_h = u16::from(!app.pending_files.is_empty());
    let reply_h = u16::from(app.reply_to.is_some());
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(5),
            Constraint::Length(reply_h),
            Constraint::Length(chips_h),
            Constraint::Length(compose_h),
            Constraint::Length(1),
        ])
        .split(cols[1]);
    draw_header(f, app, rows[0]);
    draw_messages(f, app, rows[1]);
    if reply_h > 0 {
        draw_reply(f, app, rows[2]);
    }
    if chips_h > 0 {
        draw_chips(f, app, rows[3]);
    }
    draw_compose(f, app, rows[4]);
    draw_status(f, app, rows[5]);

    match app.mode {
        Mode::Emoji => draw_emoji_picker(f, app, area),
        Mode::Contacts => draw_contact_picker(f, app, area),
        Mode::Attach => draw_attach(f, app, area),
        Mode::Search => draw_search(f, app, area),
        Mode::React => draw_react(f, app, area),
        Mode::Help => draw_help(f, area),
        _ => {}
    }
}

fn sidebar_width(total: u16) -> u16 {
    (total * 30 / 100).clamp(24, 44)
}

fn draw_sidebar(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Chats && app.mode == Mode::Normal;
    let title = if app.mode == Mode::ChatFilter || !app.chat_filter.is_empty() { format!(" / {}▏", app.chat_filter) } else { " Messages ".to_string() };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if focused || app.mode == Mode::ChatFilter { Style::default().fg(BLUE) } else { Style::default().fg(DIM) })
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let w = inner.width as usize;
    let idxs = app.filtered_chats();
    let mut items = Vec::with_capacity(idxs.len() + 1);
    if let Some(nc) = &app.new_chat {
        items.push(ListItem::new(Text::from(vec![
            Line::from(Span::styled(format!("✎ New: {}", nc.name), Style::default().fg(ACCENT).bold())),
            Line::from(Span::styled(truncate(&nc.uri, w.saturating_sub(2)), Style::default().fg(DIM))),
        ])));
    }
    for i in &idxs {
        let c = &app.chats[*i];
        let title = app.chat_title(c);
        let time = short_time(c.last_message_at.unwrap_or(0));
        let tw = time.width();
        let mut prefix = String::new();
        if c.pinned {
            prefix.push('📌');
        }
        if c.is_group {
            prefix.push('👥');
        }
        let name_w = w.saturating_sub(tw + 4 + prefix.width());
        let dot = if c.unread_count > 0 { "●" } else { " " };
        let name_style = if c.unread_count > 0 { Style::default().bold() } else { Style::default() };
        let l1 = vec![
            Span::styled(dot, Style::default().fg(BLUE)),
            Span::raw(" "),
            Span::raw(prefix),
            Span::styled(pad(&truncate(&title, name_w), name_w), name_style),
            Span::raw(" "),
            Span::styled(time, Style::default().fg(DIM)),
        ];
        let mut prev = app.previews.get(&c.id.0).map(|p| p.text.clone()).unwrap_or_default();
        if let Some(who) = app.typing_in(&c.id.0) {
            prev = format!("{} is typing…", first_name(&who));
        } else if app.previews.get(&c.id.0).is_some_and(|p| p.from_me) && !prev.is_empty() {
            prev = format!("You: {prev}");
        }
        let l2 = Line::from(Span::styled(format!("  {}", truncate(&prev, w.saturating_sub(2))), Style::default().fg(DIM)));
        items.push(ListItem::new(Text::from(vec![Line::from(l1), l2])));
    }
    let mut state = ListState::default();
    let sel = if app.new_chat.is_some() {
        Some(0)
    } else if idxs.is_empty() {
        None
    } else {
        Some(app.chat_sel.min(idxs.len() - 1))
    };
    state.select(sel);
    let list = List::new(items).highlight_style(Style::default().bg(HILITE));
    f.render_stateful_widget(list, inner, &mut state);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let (title, sub) = if let Some(nc) = &app.new_chat {
        (format!("New message to {}", nc.name), nc.uri.clone())
    } else if let Some(c) = app.current_chat() {
        let members = if c.is_group {
            c.participants.iter().map(|p| app.name(p)).collect::<Vec<_>>().join(", ")
        } else {
            c.participants.first().map(|p| p.address().to_string()).unwrap_or_default()
        };
        let service = match app.previews.get(&c.id.0).map(|p| p.service) {
            Some(Service::Sms) => "SMS",
            _ => "iMessage",
        };
        let sub = match app.typing_in(&c.id.0) {
            Some(who) => format!("{who} is typing…"),
            None => format!("{service} · {members}"),
        };
        (app.chat_title(c), sub)
    } else {
        ("imsg".to_string(), "select a conversation (Ctrl+N: someone new)".to_string())
    };
    let text = Text::from(vec![
        Line::from(Span::styled(title, Style::default().bold())),
        Line::from(Span::styled(truncate(&sub, area.width as usize), Style::default().fg(DIM))),
    ]);
    f.render_widget(Paragraph::new(text), area);
}

pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub ranges: Vec<(usize, usize)>,
}

fn draw_messages(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Messages && app.mode == Mode::Normal;
    let block = Block::default().borders(Borders::TOP).border_style(if focused { Style::default().fg(BLUE) } else { Style::default().fg(DIM) });
    let inner = block.inner(area);
    f.render_widget(block, area);
    let width = inner.width as usize;
    let height = inner.height as usize;
    let is_group = app.current_chat().map(|c| c.is_group).unwrap_or(false);
    let msgs: Vec<Message> = app.current_messages().to_vec();
    let (loading, has_more) = match app.current_chat_id() {
        Some(id) => (app.loading.contains(&id), app.has_more(&id)),
        None => (false, false),
    };
    let r = render_messages(app, &msgs, width, is_group, app.msg_sel, loading, has_more);
    app.msg_total_lines = r.lines.len();
    app.msg_view_height = height;
    app.msg_line_ranges = r.ranges;
    let total = r.lines.len();
    let max_scroll = total.saturating_sub(height);
    if app.scroll > max_scroll {
        app.scroll = max_scroll;
    }
    let offset = total.saturating_sub(height + app.scroll);
    let p = Paragraph::new(Text::from(r.lines)).scroll((offset as u16, 0));
    f.render_widget(p, inner);
}

pub fn render_messages(app: &App, msgs: &[Message], width: usize, is_group: bool, sel: Option<usize>, loading: bool, has_more: bool) -> Rendered {
    let mut lines: Vec<Line<'static>> = vec![];
    let mut ranges = vec![];
    if loading {
        lines.push(Line::from(Span::styled("loading…", Style::default().fg(DIM))).alignment(Alignment::Center));
    } else if has_more {
        lines.push(Line::from(Span::styled("↑ scroll for earlier messages", Style::default().fg(DIM))).alignment(Alignment::Center));
    }
    let max_bubble = (width * 65 / 100).clamp(12, width.saturating_sub(4).max(12));
    let mut last_date: u64 = 0;
    let last_from_me_idx = msgs.iter().rposition(|m| m.is_from_me);
    for (i, m) in msgs.iter().enumerate() {
        let start = lines.len();
        if m.sent_at.saturating_sub(last_date) > 60 * 60 * 1000 {
            lines.push(Line::from(Span::styled(format!(" {} ", full_time(m.sent_at)), Style::default().fg(DIM))).alignment(Alignment::Center));
        }
        last_date = m.sent_at;
        let selected = sel == Some(i);
        // Colour per message, like Messages.app: one conversation mixes
        // iMessage (blue) with SMS/RCS (green) over time.
        let green = m.is_from_me && m.service == Service::Sms;
        let color = if m.is_from_me {
            if green { GREEN } else { BLUE }
        } else {
            GRAY
        };
        // White on the green bubble is hard to read in most terminals.
        let fg = if green { GREEN_FG } else { Color::White };
        let muted = if green { GREEN_MUTED } else { Color::Rgb(220, 220, 220) };
        // Sender name in groups
        if is_group && !m.is_from_me {
            lines.push(Line::from(Span::styled(format!("  {}", app.name(&m.sender)), Style::default().fg(DIM))));
        }
        // Reply quote
        if let Some(rid) = &m.reply_to {
            let q = match msgs.iter().find(|x| x.id == *rid) {
                Some(orig) => format!("↩ {}: {}", app.name(&orig.sender), view::snippet(orig)),
                None => "↩ reply to an earlier message".to_string(),
            };
            let q = truncate(&q, max_bubble + 2);
            let pad_l = if m.is_from_me { width.saturating_sub(q.width() + 1) } else { 2 };
            lines.push(Line::from(vec![Span::raw(" ".repeat(pad_l)), Span::styled(q, Style::default().fg(DIM).italic())]));
        }
        // Body segments
        let mut segs: Vec<(String, Style)> = vec![];
        let base = Style::default().fg(fg).bg(color);
        if let Some(s) = m.subject.as_deref().filter(|s| !s.trim().is_empty()) {
            segs.push((format!("{s}\n"), base.bold()));
        }
        if m.unsent {
            segs.push(("(message unsent)".into(), base.italic()));
        } else {
            let only_link = view::is_only_link(m);
            let newline = |segs: &mut Vec<(String, Style)>| {
                if !segs.is_empty() && !segs.last().map(|s| s.0.ends_with('\n')).unwrap_or(true) {
                    segs.push(("\n".into(), base));
                }
            };
            for p in &m.parts {
                match p {
                    MessagePart::Text { text } => {
                        if !only_link {
                            segs.push((text.clone(), base));
                        }
                    }
                    MessagePart::Mention { text, .. } => segs.push((text.clone(), base.bold())),
                    MessagePart::Attachment { mime, filename, size, .. } => {
                        newline(&mut segs);
                        segs.push((format!("{} {} ({})", att_icon(mime), filename, human_size(*size)), base.underlined()));
                        segs.push(("\n".into(), base));
                    }
                    MessagePart::Unsupported { description } => {
                        newline(&mut segs);
                        segs.push((format!("[{description}]"), base.italic()));
                    }
                    MessagePart::Link { url, title, summary, site_name, .. } => {
                        newline(&mut segs);
                        if let Some(t) = title.as_deref().filter(|t| !t.trim().is_empty()) {
                            segs.push((format!("{}\n", t.trim()), base.bold()));
                        }
                        let site = site_name.clone().filter(|s| !s.trim().is_empty()).or_else(|| link_host(url));
                        if let Some(s) = site {
                            segs.push((format!("{s}\n"), base.italic()));
                        }
                        if let Some(s) = summary.as_deref().filter(|s| !s.trim().is_empty()) {
                            segs.push((format!("{}\n", truncate(s.trim(), 200)), base));
                        }
                        segs.push((url.clone(), base.underlined()));
                    }
                }
            }
            if !m.verified {
                segs.push(("\n⚠ unverified sender".into(), base.italic()));
            }
            if let Some(e) = &m.effect {
                segs.push((format!("\n✨ sent with {}", view::effect_name(e)), base.italic()));
            }
            if m.edited {
                segs.push(("\nEdited".into(), base.italic().fg(muted)));
            }
        }
        if segs.is_empty() {
            let sending = m.is_from_me && m.delivery == DeliveryState::Pending;
            segs.push(((if sending { "Sending…" } else { "(no content)" }).into(), base.italic()));
        }
        let wrapped = wrap_segments(&segs, max_bubble);
        let bw = wrapped.iter().map(|l| l.iter().map(|s| s.content.width()).sum::<usize>()).max().unwrap_or(1) + 2;
        let pad_l = if m.is_from_me { width.saturating_sub(bw + 1) } else { 2 };
        let marker = if selected { Span::styled("▶", Style::default().fg(ACCENT)) } else { Span::raw(" ") };
        // Top edge
        lines.push(Line::from(vec![Span::raw(" ".repeat(pad_l.saturating_sub(1))), marker, Span::styled("▄".repeat(bw), Style::default().fg(color))]));
        let n = wrapped.len();
        for (li, l) in wrapped.into_iter().enumerate() {
            let used: usize = l.iter().map(|s| s.content.width()).sum();
            let mut spans = vec![Span::raw(" ".repeat(pad_l)), Span::styled(" ", base)];
            spans.extend(l);
            spans.push(Span::styled(" ".repeat(bw.saturating_sub(used + 1)), base));
            if li + 1 == n {
                // time next to the last row
                let t = short_clock(m.sent_at);
                if m.is_from_me {
                    let tl = Span::styled(t, Style::default().fg(DIM));
                    let mut v = vec![Span::raw(" ".repeat(pad_l.saturating_sub(tl.content.width() + 1))), tl, Span::raw(" ")];
                    v.extend(spans.into_iter().skip(1));
                    spans = v;
                } else {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(t, Style::default().fg(DIM)));
                }
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(vec![Span::raw(" ".repeat(pad_l)), Span::styled("▀".repeat(bw), Style::default().fg(color))]));
        // Reactions
        let reactions: Vec<String> = m.reactions.iter().filter(|r| r.added).map(|r| format!("{} {}", view::reaction_emoji(&r.kind), first_name(&app.name(&r.sender)))).collect();
        if !reactions.is_empty() {
            let s = reactions.join("  ");
            let pl = if m.is_from_me { width.saturating_sub(s.width() + 1) } else { 3 };
            lines.push(Line::from(vec![Span::raw(" ".repeat(pl)), Span::styled(s, Style::default().fg(Color::Rgb(200, 200, 205)))]));
        }
        // Delivery status under the latest own message (and any failure)
        if m.is_from_me && (last_from_me_idx == Some(i) || m.delivery == DeliveryState::Failed) {
            let s = view::delivery_label(m.delivery);
            let st = if m.delivery == DeliveryState::Failed { Style::default().fg(Color::Red) } else { Style::default().fg(DIM) };
            lines.push(Line::from(vec![Span::raw(" ".repeat(width.saturating_sub(s.width() + 1))), Span::styled(s, st)]));
        }
        lines.push(Line::from(""));
        ranges.push((start, lines.len().saturating_sub(1)));
    }
    if msgs.is_empty() && !loading {
        lines.push(Line::from(Span::styled("No messages yet", Style::default().fg(DIM))).alignment(Alignment::Center));
    }
    Rendered { lines, ranges }
}

/// Style-preserving greedy word wrap. Returns rows of spans.
fn wrap_segments(segs: &[(String, Style)], max_w: usize) -> Vec<Vec<Span<'static>>> {
    let max_w = max_w.max(4);
    let mut rows: Vec<Vec<Span<'static>>> = vec![vec![]];
    let mut cur_w = 0usize;
    let push = |rows: &mut Vec<Vec<Span<'static>>>, cur_w: &mut usize, s: &str, st: Style| {
        if s.is_empty() {
            return;
        }
        rows.last_mut().unwrap().push(Span::styled(s.to_string(), st));
        *cur_w += s.width();
    };
    for (text, st) in segs {
        let mut first_line = true;
        for line in text.split('\n') {
            if !first_line {
                rows.push(vec![]);
                cur_w = 0;
            }
            first_line = false;
            // tokens: words and spaces
            let mut tok = String::new();
            let mut tokens: Vec<String> = vec![];
            for ch in line.chars() {
                if ch == ' ' {
                    if !tok.is_empty() {
                        tokens.push(std::mem::take(&mut tok));
                    }
                    tokens.push(" ".into());
                } else {
                    tok.push(ch);
                }
            }
            if !tok.is_empty() {
                tokens.push(tok);
            }
            for t in tokens {
                let tw = t.width();
                if t == " " {
                    if cur_w == 0 {
                        continue;
                    }
                    if cur_w + 1 > max_w {
                        rows.push(vec![]);
                        cur_w = 0;
                        continue;
                    }
                    push(&mut rows, &mut cur_w, " ", *st);
                    continue;
                }
                if cur_w + tw <= max_w {
                    push(&mut rows, &mut cur_w, &t, *st);
                } else if tw <= max_w {
                    trim_row(rows.last_mut().unwrap());
                    rows.push(vec![]);
                    cur_w = 0;
                    push(&mut rows, &mut cur_w, &t, *st);
                } else {
                    // long token: split by chars
                    let mut piece = String::new();
                    for ch in t.chars() {
                        let cw = UnicodeWidthStr::width(ch.to_string().as_str());
                        if cur_w + piece.width() + cw > max_w {
                            push(&mut rows, &mut cur_w, &piece, *st);
                            piece.clear();
                            rows.push(vec![]);
                            cur_w = 0;
                        }
                        piece.push(ch);
                    }
                    push(&mut rows, &mut cur_w, &piece, *st);
                }
            }
        }
    }
    for r in rows.iter_mut() {
        trim_row(r);
    }
    // Remove trailing empty rows but keep at least one.
    while rows.len() > 1 && rows.last().map(|r| r.is_empty()).unwrap_or(false) {
        rows.pop();
    }
    rows
}

fn trim_row(row: &mut Vec<Span<'static>>) {
    while let Some(last) = row.last_mut() {
        let t = last.content.trim_end().to_string();
        if t.is_empty() {
            row.pop();
        } else {
            last.content = t.into();
            break;
        }
    }
}

fn draw_reply(f: &mut Frame, app: &App, area: Rect) {
    let Some(r) = &app.reply_to else { return };
    let line = Line::from(vec![
        Span::styled(" ↩ Replying to ", Style::default().fg(DIM)),
        Span::styled(truncate(&r.quote.replace('\n', " "), area.width.saturating_sub(40) as usize), Style::default().fg(Color::White)),
        Span::styled("  (Esc cancels)", Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_chips(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(" 📎 ", Style::default().fg(DIM))];
    for p in &app.pending_files {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        spans.push(Span::styled(format!(" {name} "), Style::default().bg(HILITE).fg(Color::White)));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled("(Backspace on empty input removes)", Style::default().fg(DIM)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_compose(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Compose && app.mode == Mode::Normal;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if focused { Style::default().fg(BLUE) } else { Style::default().fg(DIM) });
    app.compose.set_block(block);
    f.render_widget(&app.compose, area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let conn = if !app.daemon_ok {
        Span::styled("○ no bus", Style::default().fg(Color::Red))
    } else if app.daemon_error.is_some() {
        Span::styled("○ no daemon", Style::default().fg(Color::Red))
    } else {
        match app.conn_state.as_str() {
            "connected" => Span::styled("● connected", Style::default().fg(GREEN)),
            "" => Span::styled("○ …", Style::default().fg(DIM)),
            other => Span::styled(format!("○ {other}"), Style::default().fg(ACCENT)),
        }
    };
    let unread = app.total_unread();
    let unread = if unread > 0 { format!(" ✉ {unread} ") } else { " ".into() };
    let keys = match (app.focus, app.mode) {
        (_, Mode::Normal) => match app.focus {
            Focus::Chats => "j/k move · Enter compose · / filter · Ctrl+N new · Ctrl+S search · ? help",
            Focus::Messages => "j/k select · o open · s save · y copy · r react · R reply · PgUp/PgDn · Esc",
            Focus::Compose => "Enter send · Alt+Enter newline · :smile: emoji · Ctrl+E picker · Ctrl+F file · Esc",
        },
        _ => "Esc cancel",
    };
    let status = truncate(&app.status, area.width as usize / 2);
    let line = Line::from(vec![conn, Span::styled(unread, Style::default().fg(DIM)), Span::styled(status, Style::default().fg(ACCENT)), Span::raw("  "), Span::styled(keys, Style::default().fg(DIM))]);
    f.render_widget(Paragraph::new(line), area);
}

fn popup(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn popup_block(title: String) -> Block<'static> {
    Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(Style::default().fg(BLUE)).title(title)
}

fn draw_emoji_picker(f: &mut Frame, app: &App, area: Rect) {
    let r = popup(area, 50, 18);
    f.render_widget(Clear, r);
    let block = popup_block(format!(" Emoji: {}▏", app.emoji_query));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let matches = app.emoji_matches();
    let h = inner.height as usize;
    let start = app.emoji_sel.saturating_sub(h.saturating_sub(1));
    let items: Vec<ListItem> = matches
        .iter()
        .skip(start)
        .take(h)
        .map(|e| {
            let codes: Vec<String> = e.shortcodes().take(2).map(|s| format!(":{s}:")).collect();
            ListItem::new(Line::from(vec![Span::raw(format!("{}  ", e.as_str())), Span::raw(e.name().to_string()), Span::styled(format!("  {}", codes.join(" ")), Style::default().fg(DIM))]))
        })
        .collect();
    let mut st = ListState::default();
    st.select(Some(app.emoji_sel - start));
    f.render_stateful_widget(List::new(items).highlight_style(Style::default().bg(HILITE)), inner, &mut st);
}

fn draw_contact_picker(f: &mut Frame, app: &App, area: Rect) {
    let r = popup(area, 60, 20);
    f.render_widget(Clear, r);
    let block = popup_block(format!(" To: {}▏", app.picker_query));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let matches = app.contact_matches();
    if matches.is_empty() {
        let msg = if app.contacts.is_empty() {
            "no contacts (imsg sync-mac copies the Mac's) — type a phone number or email"
        } else {
            "no matches — type a phone number or email"
        };
        f.render_widget(Paragraph::new(Span::styled(msg, Style::default().fg(DIM))).wrap(Wrap { trim: true }), inner);
        return;
    }
    let h = inner.height as usize;
    let start = app.picker_sel.saturating_sub(h.saturating_sub(1));
    let items: Vec<ListItem> = matches.iter().skip(start).take(h).map(|(n, a)| ListItem::new(Line::from(vec![Span::raw(n.clone()), Span::styled(format!("  {a}"), Style::default().fg(DIM))]))).collect();
    let mut st = ListState::default();
    st.select(Some(app.picker_sel - start));
    f.render_stateful_widget(List::new(items).highlight_style(Style::default().bg(HILITE)), inner, &mut st);
}

fn draw_attach(f: &mut Frame, app: &App, area: Rect) {
    let r = popup(area, 70, 3);
    f.render_widget(Clear, r);
    let block = popup_block(" Attach file (path, Tab completes) ".into());
    let inner = block.inner(r);
    f.render_widget(block, r);
    f.render_widget(Paragraph::new(format!("{}▏", app.attach_input)), inner);
}

fn draw_react(f: &mut Frame, app: &App, area: Rect) {
    let r = popup(area, 64, 4);
    f.render_widget(Clear, r);
    let block = popup_block(format!(" React: {}▏ ", app.react_input));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let text = Text::from(vec![
        Line::from("1 ❤️ love   2 👍 like   3 👎 dislike   4 😂 laugh   5 ‼️ emphasize   6 ❓ question"),
        Line::from(Span::styled("or type an emoji / :shortcode: and press Enter; the same one again removes it", Style::default().fg(DIM))),
    ]);
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), inner);
}

fn draw_search(f: &mut Frame, app: &App, area: Rect) {
    let r = popup(area, 80, 22);
    f.render_widget(Clear, r);
    let block = popup_block(format!(" Search messages: {}▏  (Enter to search / jump) ", app.search_query));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let h = inner.height as usize;
    let start = app.search_sel.saturating_sub(h.saturating_sub(1));
    let items: Vec<ListItem> = app
        .search_results
        .iter()
        .skip(start)
        .take(h)
        .map(|m| {
            let who = if m.is_from_me { "You".to_string() } else { app.name(&m.sender) };
            let chat = app.chats.iter().find(|c| c.id == m.chat_id).map(|c| app.chat_title(c)).unwrap_or_else(|| m.chat_id.0.clone());
            ListItem::new(Line::from(vec![
                Span::styled(short_time(m.sent_at), Style::default().fg(DIM)),
                Span::raw(" "),
                Span::styled(truncate(&chat, 18), Style::default().fg(BLUE)),
                Span::styled(format!(" {who}: "), Style::default().fg(DIM)),
                Span::raw(truncate(&m.text.clone().unwrap_or_default().replace('\n', " "), inner.width as usize)),
            ]))
        })
        .collect();
    let mut st = ListState::default();
    if !app.search_results.is_empty() {
        st.select(Some(app.search_sel - start));
    }
    f.render_stateful_widget(List::new(items).highlight_style(Style::default().bg(HILITE)), inner, &mut st);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let r = popup(area, 76, 26);
    f.render_widget(Clear, r);
    let block = popup_block(" imsg-tui keys (any key closes) ".into());
    let inner = block.inner(r);
    f.render_widget(block, r);
    let text = "\
Global
  Tab / Shift+Tab   cycle chats → messages → compose
  Ctrl+N            new message (contact picker)
  Ctrl+E            emoji picker   (or type :shortcode: inline)
  Ctrl+F            attach a file by path
  Ctrl+S            search all messages
  Ctrl+R            reload (chats, messages, contacts)   Ctrl+Q / Ctrl+C quit

Chats                        Messages
  j/k or ↑/↓  move             j/k or ↑/↓  select bubble
  Enter       compose          o / Enter   open attachment / link
  /           filter           s           save attachment → ~/Downloads
  Esc         clear filter     y           copy text
                               r           react (tapback or emoji)
                               R           reply to the selected message
                               PgUp/PgDn   scroll,  g/G  top/bottom

Compose
  Enter       send             Alt+Enter / Ctrl+J   newline
  Esc         cancel reply, then back to messages
  Backspace on empty input removes the last attachment
  Ctrl+L      redraw screen";
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

// ----- helpers -----

pub fn truncate(s: &str, w: usize) -> String {
    if s.width() <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = UnicodeWidthStr::width(ch.to_string().as_str());
        if used + cw + 1 > w {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

fn pad(s: &str, w: usize) -> String {
    let sw = s.width();
    if sw >= w { s.to_string() } else { format!("{s}{}", " ".repeat(w - sw)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::contacts::Contacts;
    use imessage_common::model::{ChatId, Handle, MessageId, Reaction, ReactionKind};
    use ratatui::{Terminal, backend::TestBackend};

    fn app() -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(Backend::new(imsg_client::Bus::Address("unix:path=/nonexistent".into())), tx, Contacts::default(), vec![])
    }

    fn msg(id: u64, from_me: bool, text: &str) -> Message {
        Message {
            id: MessageId::new(format!("M{id}")),
            chat_id: ChatId("mailto:bob@example.com".into()),
            sender: Handle::new(if from_me { "me@example.com" } else { "bob@example.com" }),
            sent_at: 1_700_000_000_000 + id * 1000,
            text: Some(text.into()),
            parts: vec![MessagePart::Text { text: text.into() }],
            subject: None,
            reply_to: None,
            is_from_me: from_me,
            delivery: DeliveryState::Delivered,
            edited: false,
            unsent: false,
            reactions: vec![],
            effect: None,
            verified: true,
            service: Service::Imessage,
        }
    }

    fn rows(r: &Rendered, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| f.render_widget(Paragraph::new(Text::from(r.lines.clone())), f.area())).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()).collect()
    }

    #[test]
    fn wide_emoji_rows_align() {
        let app = app();
        let msgs = vec![msg(1, false, "Ok. Miss you and love you 😘"), msg(2, false, "plain")];
        let r = render_messages(&app, &msgs, 60, false, None, false, false);
        let rows = rows(&r, 60, 12);
        // Every bubble edge row must start at the same column as its content row.
        let top = rows[1].find('▄').unwrap();
        let bottom = rows[3].find('▀').unwrap();
        assert_eq!(top, bottom, "top/bottom edge misaligned around wide emoji");
    }

    #[test]
    fn bubble_colour_follows_each_message_service() {
        let app = app();
        let mut sms = msg(2, true, "green");
        sms.service = Service::Sms;
        let mut theirs = msg(4, false, "theirs");
        theirs.service = Service::Sms;
        let msgs = vec![msg(1, true, "blue"), sms, theirs];
        let r = render_messages(&app, &msgs, 60, false, None, false, false);
        let style_of = |needle: &str| r.lines.iter().flat_map(|l| l.spans.iter()).find(|sp| sp.content.contains(needle)).map(|sp| sp.style).expect(needle);
        assert_eq!(style_of("blue").bg, Some(BLUE));
        assert_eq!(style_of("blue").fg, Some(Color::White));
        assert_eq!(style_of("green").bg, Some(GREEN));
        assert_eq!(style_of("green").fg, Some(GREEN_FG), "dark text on green bubbles");
        assert_eq!(style_of("theirs").bg, Some(GRAY), "incoming messages stay gray whatever the service");
    }

    #[test]
    fn parts_delivery_reactions_and_replies_render() {
        let mut app = app();
        app.me.insert("mailto:me@example.com".into());
        let mut first = msg(1, false, "look https://example.com/a");
        first.parts.push(MessagePart::Link { url: "https://example.com/a".into(), original_url: None, title: Some("A story".into()), summary: None, site_name: None, image_attachment_id: None, icon_attachment_id: None });
        first.reactions.push(Reaction { kind: ReactionKind::Love, sender: Handle::new("me@example.com"), added: true });
        first.reactions.push(Reaction { kind: ReactionKind::Emoji("🔥".into()), sender: Handle::new("bob@example.com"), added: false });
        let mut failed = msg(2, true, "late");
        failed.delivery = DeliveryState::Failed;
        let mut mine = msg(3, true, "");
        mine.parts = vec![MessagePart::Attachment { attachment_id: imessage_common::model::AttachmentId("M3:1".into()), mime: "image/jpeg".into(), filename: "IMG_1.jpg".into(), size: 183_422 }];
        mine.text = None;
        mine.reply_to = Some(MessageId::new("M1"));
        mine.delivery = DeliveryState::Read;
        mine.effect = Some("com.apple.messages.effect.CKConfettiEffect".into());
        mine.edited = true;
        let mut gone = msg(4, false, "x");
        gone.unsent = true;
        gone.text = None;
        gone.parts.clear();
        let r = render_messages(&app, &[first, failed, mine, gone], 80, false, None, false, true);
        let all: String = r.lines.iter().map(|l| l.spans.iter().map(|s| s.content.to_string()).collect::<String>() + "\n").collect();
        assert!(all.contains("↑ scroll for earlier messages"));
        assert!(all.contains("A story") && all.contains("example.com") && all.contains("look https://example.com/a"), "{all}");
        assert!(all.contains("❤️ You"), "my reaction uses You: {all}");
        assert!(!all.contains("🔥"), "removed reactions are not shown");
        assert!(all.contains("↩ bob@example.com: look https://example.com/a"), "{all}");
        assert!(all.contains("🖼 IMG_1.jpg (183 KB)"));
        assert!(all.contains("sent with confetti") && all.contains("Edited"));
        assert!(all.contains("Not delivered"), "a failed send is always marked: {all}");
        assert!(all.contains("Read"), "the latest own message shows its state: {all}");
        assert!(all.contains("(message unsent)"));
        assert_eq!(r.ranges.len(), 4);
    }
}

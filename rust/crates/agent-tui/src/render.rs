use crate::editor::cell_width;
use crate::model::{Theme, TuiModel};
use agent_core::types::{Content, Message};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualLine {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

/// Lossless byte mapping for editor wrapping; terminal cells follow grapheme widths.
pub fn wrap_editor(text: &str, width: usize) -> Vec<VisualLine> {
    let width = width.max(1);
    let mut lines = vec![];
    let mut start = 0;
    let mut cells = 0;
    let mut rendered = String::new();
    for (index, grapheme) in text.grapheme_indices(true) {
        if grapheme == "\n" {
            lines.push(VisualLine {
                text: std::mem::take(&mut rendered),
                start,
                end: index,
            });
            start = index + 1;
            cells = 0;
            continue;
        }
        let mut shown = if grapheme == "\t" {
            " ".repeat(4 - cells % 4)
        } else {
            display_text(grapheme)
        };
        let mut size = UnicodeWidthStr::width(shown.as_str());
        if cells + size > width && !rendered.is_empty() {
            lines.push(VisualLine {
                text: std::mem::take(&mut rendered),
                start,
                end: index,
            });
            start = index;
            cells = 0;
            if grapheme == "\t" {
                shown = "    ".into();
                size = 4;
            }
        }
        rendered.push_str(&shown);
        cells += size;
    }
    lines.push(VisualLine {
        text: rendered,
        start,
        end: text.len(),
    });
    lines
}

fn display_text(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '\n' | '\t' => character,
            character if character.is_control() => {
                char::from_u32(0x2400 + (character as u32).min(0x7f)).unwrap_or('\u{fffd}')
            }
            _ => character,
        })
        .collect()
}

fn add_text(lines: &mut Vec<Line<'static>>, text: &str, style: Style) {
    let mut code = false;
    for line in display_text(text).lines() {
        if line.starts_with("```") {
            code = !code;
            lines.push(Line::styled(line.to_owned(), style.fg(Color::DarkGray)));
        } else if code {
            lines.push(Line::styled(line.to_owned(), style.fg(Color::Yellow)));
        } else if line.starts_with('#') {
            lines.push(Line::styled(
                line.to_owned(),
                style.add_modifier(Modifier::BOLD),
            ));
        } else {
            lines.push(Line::styled(line.to_owned(), style));
        }
    }
}

pub fn transcript(model: &TuiModel) -> Vec<Line<'static>> {
    let mut lines = vec![];
    let color = if model.theme == Theme::Dark {
        Color::White
    } else {
        Color::Black
    };
    let style = Style::default().fg(color);
    for message in &model.messages {
        if matches!(message, Message::ToolResult(_)) && !model.show_tools {
            continue;
        }
        let title = match message {
            Message::User(_) => "YOU",
            Message::Assistant(_) => "AGENT",
            Message::ToolResult(result) => result.tool_name.as_str(),
            Message::Custom { kind, .. } => kind.as_str(),
        };
        lines.push(Line::styled(
            format!("── {} ──", display_text(title)),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        add_text(&mut lines, &message.text(), style);
        let content = match message {
            Message::User(message) => Some(&message.content),
            Message::Assistant(message) => Some(&message.content),
            Message::ToolResult(message) => Some(&message.content),
            _ => None,
        };
        if let Some(content) = content {
            for block in content {
                match block {
                    Content::Image { mime_type, .. } => lines.push(Line::styled(
                        format!("[image: {}]", display_text(mime_type)),
                        Style::default().fg(Color::Magenta),
                    )),
                    Content::Thinking { thinking } => {
                        lines.push(Line::styled(
                            "[thinking]",
                            Style::default().fg(Color::DarkGray),
                        ));
                        add_text(&mut lines, thinking, style.fg(Color::DarkGray));
                    }
                    Content::ToolCall(call) if model.show_tools => add_text(
                        &mut lines,
                        &format!("{} {}", call.name, call.arguments),
                        style.fg(Color::Yellow),
                    ),
                    _ => {}
                }
            }
        }
        if let Message::Assistant(message) = message {
            if let Some(error) = &message.error_message {
                add_text(&mut lines, error, style.fg(Color::Red));
            }
        }
        if let Message::ToolResult(result) = message {
            if let Some(diff) = result.details["diff"].as_str() {
                add_text(&mut lines, diff, style.fg(Color::Green));
            }
        }
        lines.push(Line::raw(""));
    }
    if let Some(partial) = &model.partial {
        lines.push(Line::styled(
            "AGENT · streaming",
            Style::default().fg(Color::Cyan),
        ));
        add_text(
            &mut lines,
            &Message::Assistant(partial.clone()).text(),
            style,
        );
    }
    if lines.is_empty() {
        lines.push(Line::styled("从一个问题开始。 /help 查看原生命令。", style));
    }
    if !model.search_query.is_empty() {
        for line in &mut lines {
            if line
                .to_string()
                .to_lowercase()
                .contains(&model.search_query.to_lowercase())
            {
                *line = line
                    .clone()
                    .style(Style::default().fg(Color::Black).bg(Color::Yellow));
            }
        }
    }
    lines
}

pub fn render(frame: &mut Frame<'_>, model: &mut TuiModel) {
    let area = frame.area();
    let bg = if model.theme == Theme::Dark {
        Color::Rgb(19, 23, 30)
    } else {
        Color::Rgb(245, 247, 249)
    };
    let fg = if model.theme == Theme::Dark {
        Color::White
    } else {
        Color::Black
    };
    frame.render_widget(Block::default().style(Style::default().bg(bg).fg(fg)), area);
    if area.width < 8 || area.height < 4 {
        frame.render_widget(Paragraph::new("窗口太小"), area);
        return;
    }
    let visual = wrap_editor(model.editor.text(), area.width.saturating_sub(2) as usize);
    let input_height = (visual.len().min(u16::MAX as usize) as u16)
        .saturating_add(2)
        .clamp(3, area.height.saturating_sub(4).clamp(3, 8));
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                display_text(&model.title),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  [{}]", display_text(&model.model))),
        ])),
        chunks[0],
    );
    let lines = transcript(model);
    let width = chunks[1].width.max(1);
    model.transcript_height = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(width);
    model.viewport_height = chunks[1].height as usize;
    let max = model
        .transcript_height
        .saturating_sub(model.viewport_height);
    let mut scroll = model.scroll_offset.unwrap_or(max).min(max);
    if !model.search_query.is_empty() && model.scroll_offset == Some(0) {
        if let Some(index) = lines.iter().position(|line| {
            line.to_string()
                .to_lowercase()
                .contains(&model.search_query.to_lowercase())
        }) {
            let text = lines[index].to_string();
            let folded = text.to_lowercase();
            let needle = model.search_query.to_lowercase();
            let matched_end = folded.find(&needle).map_or(0, |start| start + needle.len());
            let mut lower_bytes = 0;
            let mut original_end = 0;
            for (byte, character) in text.char_indices() {
                lower_bytes += character.to_lowercase().map(char::len_utf8).sum::<usize>();
                original_end = byte + character.len_utf8();
                if lower_bytes >= matched_end {
                    break;
                }
            }
            let mut prefix = lines[..index].to_vec();
            prefix.push(Line::raw(text[..original_end].to_owned()));
            scroll = Paragraph::new(prefix)
                .wrap(Wrap { trim: false })
                .line_count(width)
                .saturating_sub(1)
                .min(max);
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        chunks[1],
    );
    let status = if model.running {
        format!("{} · {}", model.status, model.activity)
    } else {
        model.status.clone()
    };
    frame.render_widget(
        Paragraph::new(display_text(&status)).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
    let input_area = chunks[3];
    let inner = Rect::new(
        input_area.x.saturating_add(1),
        input_area.y.saturating_add(1),
        input_area.width.saturating_sub(2),
        input_area.height.saturating_sub(2),
    );
    frame.render_widget(
        Block::bordered()
            .title(if model.running {
                "输入指令 / Enter 排队"
            } else {
                "任务"
            })
            .border_style(Style::default().fg(Color::Cyan)),
        input_area,
    );
    let cursor_line = visual
        .iter()
        .enumerate()
        .rev()
        .find(|(_, line)| model.editor.cursor() >= line.start && model.editor.cursor() <= line.end)
        .map_or(0, |(index, _)| index);
    let first = cursor_line.saturating_sub(inner.height.saturating_sub(1) as usize);
    let visible = visual
        .iter()
        .skip(first)
        .take(inner.height as usize)
        .map(|line| {
            let mut cells = 0;
            let mut spans = vec![];
            for (index, grapheme) in
                model.editor.text()[line.start..line.end].grapheme_indices(true)
            {
                let shown = if grapheme == "\t" {
                    " ".repeat(4 - cells % 4)
                } else {
                    display_text(grapheme)
                };
                cells += UnicodeWidthStr::width(shown.as_str());
                let selected = model.editor.selection().is_some_and(|(start, end)| {
                    line.start + index < end && line.start + index + grapheme.len() > start
                });
                spans.push(Span::styled(
                    shown,
                    if selected {
                        Style::default().bg(Color::Blue).fg(Color::White)
                    } else {
                        Style::default()
                    },
                ));
            }
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible), inner);
    if inner.width > 0 && inner.height > 0 {
        if let Some(line) = visual.get(cursor_line) {
            let prefix = &model.editor.text()[line.start..model.editor.cursor().min(line.end)];
            let column =
                cell_width(&display_text(prefix)).min(inner.width.saturating_sub(1) as usize);
            frame.set_cursor_position((
                inner.x + column as u16,
                inner.y + (cursor_line - first) as u16,
            ));
        }
    }
    frame.render_widget(
        Paragraph::new(format!(
            "{} 发送 · {} 换行 · {} 停止 · {} 会话 · {} 退出",
            model.keybindings.label("submit"),
            model.keybindings.label("newline"),
            model.keybindings.label("cancel"),
            model.keybindings.label("sessions"),
            model.keybindings.label("quit")
        ))
        .style(Style::default().fg(Color::DarkGray)),
        chunks[4],
    );
    if let Some(selected) = model.session_selector {
        let rect = overlay(area);
        frame.render_widget(Clear, rect);
        let items = model
            .sessions
            .iter()
            .map(|session| ListItem::new(display_text(&session.name)))
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected((!items.is_empty()).then_some(selected));
        frame.render_stateful_widget(
            List::new(items)
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "会话 · {} 选择 / {} 关闭",
                    model.keybindings.label("submit"),
                    model.keybindings.label("cancel")
                )))
                .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
                .style(Style::default().bg(bg).fg(fg)),
            rect,
            &mut state,
        );
    }
    if let Some(search) = &model.search_input {
        let rect = Rect::new(area.x, area.y, area.width, 3.min(area.height));
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(display_text(search))
                .block(Block::bordered().title(format!(
                    "搜索 · {} 确认 / {} 取消",
                    model.keybindings.label("submit"),
                    model.keybindings.label("cancel")
                )))
                .style(Style::default().bg(bg).fg(fg)),
            rect,
        );
    }
}
fn overlay(area: Rect) -> Rect {
    let width = area.width.saturating_sub(4).min(70);
    let height = area.height.saturating_sub(4).min(18);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

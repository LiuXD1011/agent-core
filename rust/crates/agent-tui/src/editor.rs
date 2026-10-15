use std::collections::VecDeque;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub const MAX_INPUT_BYTES: usize = 1_048_576;

pub fn cell_width(text: &str) -> usize {
    text.graphemes(true).fold(0, |width, grapheme| {
        width
            + if grapheme == "\t" {
                4 - width % 4
            } else {
                UnicodeWidthStr::width(grapheme)
            }
    })
}

#[derive(Clone, Debug)]
struct Snapshot {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct Editor {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
    undo: VecDeque<Snapshot>,
    redo: Vec<Snapshot>,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    kill_ring: VecDeque<String>,
    yank_range: Option<(usize, usize)>,
    preferred_column: Option<usize>,
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    pub fn selection(&self) -> Option<(usize, usize)> {
        self.anchor
            .filter(|anchor| *anchor != self.cursor)
            .map(|anchor| (anchor.min(self.cursor), anchor.max(self.cursor)))
    }
    pub fn selected_text(&self) -> Option<&str> {
        self.selection().map(|(start, end)| &self.text[start..end])
    }
    pub fn set_text(&mut self, text: impl Into<String>) -> Result<(), String> {
        let text = text.into().replace("\r\n", "\n").replace('\r', "\n");
        if text.len() > MAX_INPUT_BYTES {
            return Err("Input exceeds 1MB".into());
        }
        self.checkpoint();
        self.text = text;
        self.cursor = self.text.len();
        self.anchor = None;
        self.preferred_column = None;
        Ok(())
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
        }
    }
    fn checkpoint(&mut self) {
        self.undo.push_back(self.snapshot());
        if self.undo.len() > 100 {
            self.undo.pop_front();
        }
        self.redo.clear();
        self.yank_range = None;
        self.preferred_column = None;
    }
    fn restore(&mut self, snapshot: Snapshot) {
        self.text = snapshot.text;
        self.cursor = snapshot.cursor;
        self.anchor = snapshot.anchor;
        self.preferred_column = None;
        self.yank_range = None;
    }
    pub fn undo(&mut self) {
        if let Some(snapshot) = self.undo.pop_back() {
            self.redo.push(self.snapshot());
            self.restore(snapshot);
        }
    }
    pub fn redo(&mut self) {
        if let Some(snapshot) = self.redo.pop() {
            self.undo.push_back(self.snapshot());
            self.restore(snapshot);
        }
    }
    fn move_to(&mut self, cursor: usize, select: bool) {
        if select {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = cursor;
        self.yank_range = None;
    }
    pub fn left(&mut self, select: bool) {
        self.preferred_column = None;
        if !select {
            if let Some((start, _)) = self.selection() {
                self.move_to(start, false);
                return;
            }
        }
        let previous = self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(index, _)| index);
        self.move_to(previous, select);
    }
    pub fn right(&mut self, select: bool) {
        self.preferred_column = None;
        if !select {
            if let Some((_, end)) = self.selection() {
                self.move_to(end, false);
                return;
            }
        }
        let next = self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |grapheme| self.cursor + grapheme.len());
        self.move_to(next, select);
    }
    pub fn home(&mut self, select: bool) {
        self.preferred_column = None;
        let start = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |position| position + 1);
        self.move_to(start, select);
    }
    pub fn end(&mut self, select: bool) {
        self.preferred_column = None;
        let end = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |position| self.cursor + position);
        self.move_to(end, select);
    }
    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }
    pub fn word_left(&mut self, select: bool) {
        self.preferred_column = None;
        let before = &self.text[..self.cursor];
        let previous = before
            .unicode_word_indices()
            .next_back()
            .map_or(0, |(position, _)| position);
        self.move_to(previous, select);
    }
    pub fn word_right(&mut self, select: bool) {
        self.preferred_column = None;
        let after = &self.text[self.cursor..];
        let next = after
            .unicode_word_indices()
            .next()
            .map_or(self.text.len(), |(position, word)| {
                self.cursor + position + word.len()
            });
        self.move_to(next, select);
    }
    pub fn vertical(&mut self, down: bool, select: bool) -> bool {
        let line_start = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |position| position + 1);
        let column = *self
            .preferred_column
            .get_or_insert_with(|| cell_width(&self.text[line_start..self.cursor]));
        let bounds = if down {
            self.text[self.cursor..].find('\n').map(|position| {
                let start = self.cursor + position + 1;
                let end = self.text[start..]
                    .find('\n')
                    .map_or(self.text.len(), |position| start + position);
                (start, end)
            })
        } else if line_start == 0 {
            None
        } else {
            let end = line_start - 1;
            let start = self.text[..end]
                .rfind('\n')
                .map_or(0, |position| position + 1);
            Some((start, end))
        };
        let Some((start, end)) = bounds else {
            return false;
        };
        let mut width = 0;
        let mut target = start;
        for (index, grapheme) in self.text[start..end].grapheme_indices(true) {
            let next = width
                + if grapheme == "\t" {
                    4 - width % 4
                } else {
                    UnicodeWidthStr::width(grapheme)
                };
            if next > column {
                break;
            }
            width = next;
            target = start + index + grapheme.len();
        }
        self.move_to(target, select);
        true
    }
    pub fn insert(&mut self, input: &str) -> Result<(), String> {
        let input = input.replace("\r\n", "\n").replace('\r', "\n");
        let (start, end) = self.selection().unwrap_or((self.cursor, self.cursor));
        if self.text.len() - (end - start) + input.len() > MAX_INPUT_BYTES {
            return Err("Input exceeds 1MB".into());
        }
        self.checkpoint();
        self.text.replace_range(start..end, &input);
        self.cursor = start + input.len();
        self.anchor = None;
        // An inserted ZWJ/combining sequence can join the following grapheme.
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .find(|index| *index >= self.cursor)
            .unwrap_or(self.text.len());
        self.history_index = None;
        Ok(())
    }
    fn delete_range(&mut self, start: usize, end: usize, kill: bool) {
        if start == end {
            return;
        }
        self.checkpoint();
        if kill {
            self.kill_ring.push_front(self.text[start..end].into());
            self.kill_ring.truncate(32);
        }
        self.text.replace_range(start..end, "");
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .find(|index| *index >= start)
            .unwrap_or(self.text.len());
        self.anchor = None;
        self.history_index = None;
    }
    pub fn backspace(&mut self) {
        if let Some((start, end)) = self.selection() {
            self.delete_range(start, end, false);
            return;
        }
        let previous = self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(index, _)| index);
        self.delete_range(previous, self.cursor, false);
    }
    pub fn delete(&mut self) {
        if let Some((start, end)) = self.selection() {
            self.delete_range(start, end, false);
            return;
        }
        let end = self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |grapheme| self.cursor + grapheme.len());
        self.delete_range(self.cursor, end, false);
    }
    pub fn kill_to_start(&mut self) {
        let start = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |position| position + 1);
        self.delete_range(start, self.cursor, true);
    }
    pub fn kill_to_end(&mut self) {
        let end = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |position| self.cursor + position);
        self.delete_range(self.cursor, end, true);
    }
    pub fn kill_word_left(&mut self) {
        let end = self.cursor;
        let start = self.text[..end]
            .unicode_word_indices()
            .next_back()
            .map_or(0, |(position, _)| position);
        self.delete_range(start, end, true);
    }
    pub fn kill_word_right(&mut self) {
        let end = self.text[self.cursor..]
            .unicode_word_indices()
            .next()
            .map_or(self.text.len(), |(position, word)| {
                self.cursor + position + word.len()
            });
        self.delete_range(self.cursor, end, true);
    }
    pub fn yank(&mut self) -> Result<(), String> {
        if let Some(text) = self.kill_ring.front().cloned() {
            let start = self.selection().map_or(self.cursor, |(start, _)| start);
            self.insert(&text)?;
            self.yank_range = Some((start, start + text.len()));
        }
        Ok(())
    }
    pub fn yank_pop(&mut self) -> Result<(), String> {
        let Some((start, end)) = self.yank_range else {
            return Ok(());
        };
        if self.kill_ring.len() < 2 {
            return Ok(());
        }
        let previous = self.kill_ring.pop_front().unwrap();
        self.kill_ring.push_back(previous);
        let next = self.kill_ring.front().unwrap().clone();
        self.anchor = Some(start);
        self.cursor = end;
        self.insert(&next)?;
        self.yank_range = Some((start, start + next.len()));
        Ok(())
    }
    pub fn submit(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.anchor = None;
        self.undo.clear();
        self.redo.clear();
        self.history_index = None;
        self.yank_range = None;
        self.preferred_column = None;
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
            if self.history.len() > 1000 {
                self.history.remove(0);
            }
        }
        text
    }
    pub fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_index {
            None => {
                self.history_draft = self.text.clone();
                self.history.len() - 1
            }
            Some(index) => index.saturating_sub(1),
        };
        self.text = self.history[index].clone();
        self.cursor = self.text.len();
        self.anchor = None;
        self.history_index = Some(index);
        self.yank_range = None;
        self.preferred_column = None;
    }
    pub fn history_next(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 >= self.history.len() {
            self.text = self.history_draft.clone();
            self.history_index = None;
        } else {
            self.text = self.history[index + 1].clone();
            self.history_index = Some(index + 1);
        }
        self.cursor = self.text.len();
        self.anchor = None;
        self.yank_range = None;
        self.preferred_column = None;
    }
}

use ratatui::text::Line;

pub(super) const DEFAULT_MAX_VISIBLE_LINES: usize = 10;
const INPUT_PREFIX: &str = "> ";

#[derive(Debug, Clone)]
pub(super) struct ComposerState {
    pub(super) buffer: String,
    cursor_byte: usize,
    pub(super) scroll_line_offset: usize,
    pub(super) max_visible_lines: usize,
    preferred_column: Option<usize>,
}

impl ComposerState {
    pub(super) fn new() -> Self {
        Self {
            buffer: String::new(),
            cursor_byte: 0,
            scroll_line_offset: 0,
            max_visible_lines: DEFAULT_MAX_VISIBLE_LINES,
            preferred_column: None,
        }
    }

    pub(super) fn input_height(&self) -> usize {
        self.logical_line_count()
            .max(1)
            .min(self.max_visible_lines.max(1))
    }

    pub(super) fn insert_char(&mut self, ch: char) {
        self.buffer.insert(self.cursor_byte, ch);
        self.cursor_byte += ch.len_utf8();
        self.preferred_column = None;
        self.ensure_cursor_visible();
    }

    pub(super) fn insert_str(&mut self, input: &str) {
        let normalized = normalize_line_endings(input);
        if normalized.is_empty() {
            return;
        }

        self.buffer.insert_str(self.cursor_byte, &normalized);
        self.cursor_byte += normalized.len();
        self.preferred_column = None;
        self.ensure_cursor_visible();
    }

    pub(super) fn newline(&mut self) {
        self.insert_char('\n');
    }

    pub(super) fn backspace(&mut self) {
        if self.cursor_byte == 0 {
            return;
        }

        let previous_byte = self.buffer[..self.cursor_byte]
            .char_indices()
            .next_back()
            .map(|(idx, _)| idx)
            .unwrap_or(0);

        self.buffer.drain(previous_byte..self.cursor_byte);
        self.cursor_byte = previous_byte;
        self.preferred_column = None;
        self.ensure_cursor_visible();
    }

    pub(super) fn cursor_left(&mut self) {
        if self.cursor_byte == 0 {
            return;
        }

        self.cursor_byte = self.buffer[..self.cursor_byte]
            .char_indices()
            .next_back()
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        self.preferred_column = None;
        self.ensure_cursor_visible();
    }

    pub(super) fn cursor_right(&mut self) {
        if self.cursor_byte >= self.buffer.len() {
            return;
        }

        let mut iter = self.buffer[self.cursor_byte..].chars();
        if let Some(next) = iter.next() {
            self.cursor_byte += next.len_utf8();
        }
        self.preferred_column = None;
        self.ensure_cursor_visible();
    }

    pub(super) fn cursor_up(&mut self) {
        let (line, col) = self.cursor_line_col();
        if line == 0 {
            return;
        }

        let desired_col = self.preferred_column.unwrap_or(col);
        self.set_cursor_line_col(line - 1, desired_col);
        self.preferred_column = Some(desired_col);
        self.ensure_cursor_visible();
    }

    pub(super) fn cursor_down(&mut self) {
        let (line, col) = self.cursor_line_col();
        let max_line = self.logical_line_count().saturating_sub(1);
        if line >= max_line {
            return;
        }

        let desired_col = self.preferred_column.unwrap_or(col);
        self.set_cursor_line_col(line + 1, desired_col);
        self.preferred_column = Some(desired_col);
        self.ensure_cursor_visible();
    }

    pub(super) fn submit(&mut self) -> Option<String> {
        if self.buffer.trim().is_empty() {
            return None;
        }

        let output = std::mem::take(&mut self.buffer);
        self.cursor_byte = 0;
        self.scroll_line_offset = 0;
        self.preferred_column = None;
        Some(output)
    }

    pub(super) fn prefixed_lines(&self) -> Vec<Line<'static>> {
        self.logical_lines()
            .into_iter()
            .map(|line| Line::from(format!("{INPUT_PREFIX}{line}")))
            .collect()
    }

    pub(super) fn cursor_screen_position(&self) -> (u16, u16) {
        let (line, col) = self.cursor_line_col();
        let rel_y = line.saturating_sub(self.scroll_line_offset);
        ((INPUT_PREFIX.chars().count() + col) as u16, rel_y as u16)
    }

    pub(super) fn ensure_cursor_visible(&mut self) {
        let visible = self.max_visible_lines.max(1);
        let (line, _) = self.cursor_line_col();

        if line < self.scroll_line_offset {
            self.scroll_line_offset = line;
            return;
        }

        if line >= self.scroll_line_offset + visible {
            self.scroll_line_offset = line + 1 - visible;
        }
    }

    fn logical_lines(&self) -> Vec<&str> {
        self.buffer.split('\n').collect()
    }

    fn logical_line_count(&self) -> usize {
        self.logical_lines().len()
    }

    fn cursor_line_col(&self) -> (usize, usize) {
        let starts = self.line_starts();
        let mut line_idx = 0;
        for (idx, start) in starts.iter().enumerate() {
            if *start > self.cursor_byte {
                break;
            }
            line_idx = idx;
        }

        let line_start = starts[line_idx];
        let col = self.buffer[line_start..self.cursor_byte].chars().count();
        (line_idx, col)
    }

    fn set_cursor_line_col(&mut self, line_idx: usize, desired_col: usize) {
        let starts = self.line_starts();
        let line_idx = line_idx.min(starts.len().saturating_sub(1));
        let line_start = starts[line_idx];
        let line_end = if line_idx + 1 < starts.len() {
            starts[line_idx + 1].saturating_sub(1)
        } else {
            self.buffer.len()
        };

        let line = &self.buffer[line_start..line_end];
        let char_len = line.chars().count();
        let target_col = desired_col.min(char_len);
        let target_offset = if target_col == char_len {
            line.len()
        } else {
            line.char_indices()
                .nth(target_col)
                .map(|(idx, _)| idx)
                .unwrap_or(line.len())
        };

        self.cursor_byte = line_start + target_offset;
    }

    fn line_starts(&self) -> Vec<usize> {
        let mut starts = vec![0];
        for (idx, ch) in self.buffer.char_indices() {
            if ch == '\n' {
                starts.push(idx + 1);
            }
        }
        starts
    }
}

pub(super) fn normalize_line_endings(input: &str) -> String {
    input.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::ComposerState;

    #[test]
    fn composer_normalizes_line_endings_on_insert_str() {
        let mut composer = ComposerState::new();
        composer.insert_str("a\r\nb\rc");
        assert_eq!(composer.buffer, "a\nb\nc");
    }
}

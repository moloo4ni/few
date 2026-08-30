#[derive(Debug, Clone)]
pub enum Completion {
    Ghost { tail: String },
    Menu { items: Vec<String>, sel: usize },
}

#[derive(Debug, Default)]
pub struct InputState {
    pub text: Vec<char>,
    pub cursor: usize,
    pub history: Vec<String>,
    hist_idx: Option<usize>,
    draft: Option<String>,
    pub mentions: Vec<(usize, usize)>,
    pub completion: Option<Completion>,
}

const WORD_STOP: [char; 4] = [' ', '\t', '\n', '\r'];

impl InputState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> String {
        self.text.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn apply_edit(&mut self, start: usize, end: usize, repl_len: usize, inserted_separator: bool) {
        let delta = repl_len as isize - (end - start) as isize;
        self.mentions.retain(|(a, b)| {
            let overlaps = start < *b && end > *a;
            let insertion_inside = start == end && *a < start && start < *b;
            let joins_path =
                start == end && repl_len > 0 && !inserted_separator && (start == *a || start == *b);
            !(overlaps || insertion_inside || joins_path)
        });
        for (a, b) in self.mentions.iter_mut() {
            if *a >= end {
                *a = (*a as isize + delta).max(0) as usize;
                *b = (*b as isize + delta).max(0) as usize;
            }
        }
        self.completion = None;
    }

    pub fn insert_str(&mut self, s: &str) {
        let chars: Vec<char> = s.chars().collect();
        let n = chars.len();
        let pos = self.cursor;
        for (i, c) in chars.iter().enumerate() {
            self.text.insert(pos + i, *c);
        }
        self.apply_edit(pos, pos, n, s.chars().all(char::is_whitespace));
        self.cursor += n;
    }

    pub fn newline(&mut self) {
        self.insert_str("\n");
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let pos = self.cursor - 1;
        self.text.remove(pos);
        self.apply_edit(pos, pos + 1, 0, false);
        self.cursor = pos;
    }

    pub fn delete_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        self.text.remove(self.cursor);
        self.apply_edit(self.cursor, self.cursor + 1, 0, false);
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        while self.cursor > 0 && self.text[self.cursor - 1] != '\n' {
            self.cursor -= 1;
        }
    }

    pub fn end(&mut self) {
        while self.cursor < self.text.len() && self.text[self.cursor] != '\n' {
            self.cursor += 1;
        }
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.hist_idx {
            None => {
                self.draft = Some(self.text());
                self.hist_idx = Some(self.history.len() - 1);
            }
            Some(i) => {
                if i > 0 {
                    self.hist_idx = Some(i - 1);
                }
            }
        }
        if let Some(i) = self.hist_idx {
            let entry = self.history[i].clone();
            self.set_text(&entry);
        }
    }

    pub fn history_next(&mut self) {
        match self.hist_idx {
            None => {}
            Some(i) => {
                if i + 1 < self.history.len() {
                    self.hist_idx = Some(i + 1);
                    let entry = self.history[i + 1].clone();
                    self.set_text(&entry);
                } else {
                    self.hist_idx = None;
                    let d = self.draft.take().unwrap_or_default();
                    self.set_text(&d);
                }
            }
        }
    }

    pub fn push_history(&mut self, entry: &str) {
        if entry.trim().is_empty() {
            return;
        }
        if self.history.last().map(|l| l == entry).unwrap_or(false) {
            return;
        }
        self.history.push(entry.to_owned());
        self.hist_idx = None;
        self.draft = None;
    }

    pub fn set_text(&mut self, s: &str) {
        self.text = s.chars().collect();
        self.cursor = self.text.len();
        self.mentions.clear();
        self.completion = None;
    }

    pub fn clear(&mut self) {
        self.set_text("");
        self.hist_idx = None;
        self.draft = None;
    }

    pub fn current_word(&self) -> Option<(usize, usize)> {
        let mut start = self.cursor;
        while start > 0 && !WORD_STOP.contains(&self.text[start - 1]) {
            start -= 1;
        }
        let mut end = self.cursor;
        while end < self.text.len() && !WORD_STOP.contains(&self.text[end]) {
            end += 1;
        }
        if end <= start {
            return None;
        }
        Some((start, end))
    }

    pub fn update_completion(&mut self, index: &[String]) {
        let Some((start, end)) = self.current_word() else {
            self.completion = None;
            return;
        };
        if self.cursor != end || end - start < 2 {
            self.completion = None;
            return;
        }
        let word: String = self.text[start..end].iter().collect();
        let lowered = word.to_lowercase();

        let mut prefix_hits: Vec<String> = Vec::new();
        let mut substring_hits: Vec<String> = Vec::new();
        for f in index {
            let fl = f.to_lowercase();
            let base = fl.rsplit('/').next().unwrap_or("");
            if fl.starts_with(&lowered) || base.starts_with(&lowered) {
                prefix_hits.push(f.clone());
            } else if base.contains(&lowered) {
                substring_hits.push(f.clone());
            }
            if prefix_hits.len() + substring_hits.len() >= 50 {
                break;
            }
        }
        let hits = if !prefix_hits.is_empty() {
            prefix_hits
        } else {
            substring_hits
        };
        match hits.len() {
            0 => self.completion = None,
            1 => {
                let wlen = word.chars().count();
                let tail: String = hits[0].chars().skip(wlen).collect();
                if tail.is_empty() {
                    self.completion = None;
                } else {
                    self.completion = Some(Completion::Ghost { tail });
                }
            }
            _ => {
                self.completion = Some(Completion::Menu {
                    items: hits,
                    sel: 0,
                });
            }
        }
    }

    fn replace_word(&mut self, replacement: &str) {
        let Some((start, end)) = self.current_word() else {
            return;
        };
        let new_chars: Vec<char> = replacement.chars().collect();
        let n = new_chars.len();
        for _ in start..end {
            let _ = self.text.remove(start);
        }
        for (i, c) in new_chars.iter().enumerate() {
            self.text.insert(start + i, *c);
        }
        self.apply_edit(start, end, n, false);
        self.mentions.push((start, start + n));
        self.cursor = start + n;
        self.completion = None;
    }

    pub fn accept_selected(&mut self) {
        match self.completion.clone() {
            Some(Completion::Ghost { tail }) => {
                let full = format!("{}{}", self.word_text(), tail);
                self.replace_word(&full);
            }
            Some(Completion::Menu { items, sel }) => {
                if let Some(item) = items.get(sel) {
                    let item = item.clone();
                    self.replace_word(&item);
                }
            }
            None => {}
        }
    }

    pub fn mentioned_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        for (start, end) in &self.mentions {
            if start < end && *end <= self.text.len() {
                let path: String = self.text[*start..*end].iter().collect();
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
        paths
    }

    pub fn cycle_menu(&mut self, forward: bool) {
        if let Some(Completion::Menu { items, sel }) = &mut self.completion {
            if items.is_empty() {
                return;
            }
            let len = items.len();
            *sel = if forward {
                (*sel + 1) % len
            } else {
                (*sel + len - 1) % len
            };
        }
    }

    pub fn menu_items(&self) -> Option<&[String]> {
        match self.completion {
            Some(Completion::Menu { ref items, .. }) => Some(items),
            _ => None,
        }
    }

    fn word_text(&self) -> String {
        match self.current_word() {
            Some((s, e)) => self.text[s..e].iter().collect(),
            None => String::new(),
        }
    }

    pub fn ghost_tail(&self) -> Option<String> {
        let (_, end) = self.current_word()?;
        if self.cursor != end {
            return None;
        }
        match &self.completion {
            Some(Completion::Ghost { tail }) => Some(tail.clone()),
            Some(Completion::Menu { items, sel }) => {
                let item = items.get(*sel)?;
                let word = self.word_text();
                let tail: String = item.chars().skip(word.chars().count()).collect();
                if tail.is_empty() {
                    None
                } else {
                    Some(tail)
                }
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_mentions_survive_separators_but_not_path_edits() {
        let index = vec!["src/main.rs".to_owned()];
        let mut input = InputState::new();
        input.set_text("src/ma");
        input.update_completion(&index);
        input.accept_selected();
        assert_eq!(input.mentioned_paths(), vec!["src/main.rs"]);

        input.insert_str(" ");
        assert_eq!(input.mentioned_paths(), vec!["src/main.rs"]);

        input.cursor = "src".chars().count();
        input.insert_str("x");
        assert!(input.mentioned_paths().is_empty());
    }

    #[test]
    fn editing_inside_a_mention_invalidates_its_range() {
        let index = vec!["notes.txt".to_owned()];
        let mut input = InputState::new();
        input.set_text("not");
        input.update_completion(&index);
        input.accept_selected();
        input.cursor = 3;
        input.delete_forward();
        assert!(input.mentioned_paths().is_empty());
    }
}

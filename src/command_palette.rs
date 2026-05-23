//! Command palette: a searchable overlay (summoned with Cmd-Shift-P) that
//! fuzzy-filters a static list of actions — "Set title", "Reload config",
//! "Zoom in", … — and runs the one you pick, so you don't have to remember a
//! keybinding or type an escape sequence by hand.
//!
//! This module is deliberately pure: no GPU, no winit, no `State`. It owns the
//! palette's *logic* (the text field, the fuzzy matcher, the command registry,
//! and the open/filter/select/argument state machine) so it can be unit-tested
//! in isolation. `main.rs` drives it from real key events, renders it with the
//! same quad/glyph batch the completion popup uses, and dispatches the chosen
//! [`PaletteAction`] against `State` in `run_palette_action`.

/// Max command rows drawn at once; longer result lists scroll. Mirrors the
/// completion popup's `COMPLETION_MAX_VISIBLE`, just a touch shorter since the
/// palette also spends a row on its input line.
pub const PALETTE_MAX_VISIBLE: usize = 8;

/// What a command does when run. The palette is a front end only — every arm
/// maps to behaviour that already exists on `State`; see `run_palette_action`
/// in `main.rs`. Adding a command is one [`Command`] entry plus one match arm
/// there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaletteAction {
    /// Set the window title to a user-typed string (routed through the same
    /// OSC-2 path a program would use, so the existing title plumbing applies).
    SetTitle,
    /// Clear a manually-set title, falling back to the cwd-derived one.
    ClearTitle,
    /// Re-read the config file and swap the color scheme live.
    ReloadConfig,
    /// Switch the active color scheme. When following the system appearance is
    /// off, this sets the single `color_scheme`; when on, it assigns the slot
    /// for the current appearance (light or dark). Picks from the scheme list.
    SetTheme,
    /// Assign the scheme used in system *light* mode (`light_scheme`). Picks
    /// from the scheme list.
    SetLightTheme,
    /// Assign the scheme used in system *dark* mode (`dark_scheme`). Picks from
    /// the scheme list.
    SetDarkTheme,
    /// Toggle following the system light/dark appearance (`auto_theme`), then
    /// apply the scheme that now matches.
    ToggleFollowSystem,
    /// Increase / decrease the font size by one point.
    ZoomIn,
    ZoomOut,
    /// Toggle the wireframe debug view.
    ToggleWireframe,
    /// Select + copy the last completed command's output (OSC 133).
    CopyLastOutput,
    /// Open a new Yutani window (a fresh process) in the current working dir.
    NewWindow,
}

/// One entry in the palette's command list.
pub struct Command {
    /// Human-readable label shown in the list and matched against the query.
    pub title: &'static str,
    /// The behaviour this entry triggers.
    pub action: PaletteAction,
    /// `Some(prompt)` if the command needs an argument before it can run
    /// (e.g. the title for `SetTitle`). Selecting such a command switches the
    /// palette into [`Mode::Argument`] / [`Mode::Choose`] rather than running
    /// immediately.
    pub arg_prompt: Option<&'static str>,
    /// When set, the argument isn't typed freely — it's picked from a list the
    /// host supplies at selection time (e.g. the available color schemes for
    /// `SetTheme`). Selecting the command emits [`Outcome::RequestChoices`]; the
    /// host enumerates the options and calls [`CommandPalette::enter_choose`],
    /// which drops the palette into [`Mode::Choose`] — a fuzzy-filtered picker
    /// over that list. This is what gives such commands autocomplete (the same
    /// matcher the command list uses) and validation (you can only pick an entry
    /// that exists). Requires `arg_prompt` to be `Some`.
    pub choose: bool,
}

/// The static command registry. Order here is the tiebreak order when several
/// commands score equally under the fuzzy filter.
pub const COMMANDS: &[Command] = &[
    Command {
        title: "Set title",
        action: PaletteAction::SetTitle,
        arg_prompt: Some("Title"),
        choose: false,
    },
    Command {
        title: "Clear title",
        action: PaletteAction::ClearTitle,
        arg_prompt: None,
        choose: false,
    },
    Command {
        title: "Reload config",
        action: PaletteAction::ReloadConfig,
        arg_prompt: None,
        choose: false,
    },
    Command {
        title: "Set theme",
        action: PaletteAction::SetTheme,
        arg_prompt: Some("Theme"),
        choose: true,
    },
    Command {
        title: "Set light theme",
        action: PaletteAction::SetLightTheme,
        arg_prompt: Some("Light theme"),
        choose: true,
    },
    Command {
        title: "Set dark theme",
        action: PaletteAction::SetDarkTheme,
        arg_prompt: Some("Dark theme"),
        choose: true,
    },
    Command {
        title: "Toggle follow system appearance",
        action: PaletteAction::ToggleFollowSystem,
        arg_prompt: None,
        choose: false,
    },
    Command {
        title: "Zoom in",
        action: PaletteAction::ZoomIn,
        arg_prompt: None,
        choose: false,
    },
    Command {
        title: "Zoom out",
        action: PaletteAction::ZoomOut,
        arg_prompt: None,
        choose: false,
    },
    Command {
        title: "Toggle wireframe",
        action: PaletteAction::ToggleWireframe,
        arg_prompt: None,
        choose: false,
    },
    Command {
        title: "Copy last output",
        action: PaletteAction::CopyLastOutput,
        arg_prompt: None,
        choose: false,
    },
    Command {
        title: "New window",
        action: PaletteAction::NewWindow,
        arg_prompt: None,
    },
];

/// A minimal single-line text input: a string plus a caret. The caret is a
/// byte offset into `value` that always sits on a `char` boundary, so the field
/// is UTF-8 safe (you can type, and delete, multi-byte characters). This is the
/// reusable editing primitive the codebase previously lacked — used here for
/// both the search query and the argument prompt.
#[derive(Default, Clone, PartialEq, Eq, Debug)]
pub struct TextField {
    pub value: String,
    /// Caret position as a byte index into `value`. Invariant: on a char
    /// boundary, and `<= value.len()`.
    pub cursor: usize,
}

impl TextField {
    /// Insert a character at the caret and advance past it.
    pub fn insert(&mut self, c: char) {
        self.value.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Delete the character before the caret (Backspace).
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.prev_boundary(self.cursor);
        self.value.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    /// Delete the character at the caret (forward Delete).
    pub fn delete(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        let next = self.next_boundary(self.cursor);
        self.value.replace_range(self.cursor..next, "");
    }

    /// Move the caret one character left.
    pub fn left(&mut self) {
        self.cursor = self.prev_boundary(self.cursor);
    }

    /// Move the caret one character right.
    pub fn right(&mut self) {
        self.cursor = self.next_boundary(self.cursor);
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.value.len();
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    /// Number of characters before the caret — used by the renderer to place
    /// the caret quad (monospace: column = char count × cell width).
    pub fn cursor_col(&self) -> usize {
        self.value[..self.cursor].chars().count()
    }

    fn prev_boundary(&self, from: usize) -> usize {
        self.value[..from]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    fn next_boundary(&self, from: usize) -> usize {
        self.value[from..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| from + i)
            .unwrap_or(self.value.len())
    }
}

/// Score `needle` against `haystack` as a case-insensitive subsequence match.
/// Returns `None` when `needle` isn't a subsequence of `haystack`; a higher
/// score means a better match. An empty needle matches everything with score 0
/// (so the unfiltered list keeps its registry order).
///
/// Scoring favours matches that feel right to a human: consecutive characters
/// and matches at word boundaries (start of string, or after a space/`-`/`_`)
/// score well; gaps between matched characters are penalised lightly.
pub fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = haystack.chars().flat_map(|c| c.to_lowercase()).collect();
    let need: Vec<char> = needle.chars().flat_map(|c| c.to_lowercase()).collect();

    let mut score = 0i32;
    let mut hi = 0usize;
    let mut prev_match: Option<usize> = None;

    for &nc in &need {
        // Advance through the haystack to the next occurrence of `nc`.
        let mut found = None;
        while hi < hay.len() {
            if hay[hi] == nc {
                found = Some(hi);
                break;
            }
            hi += 1;
        }
        let m = found?; // not a subsequence

        score += 1; // base reward per matched char
        let at_boundary = m == 0 || matches!(hay[m - 1], ' ' | '-' | '_' | '/' | '.');
        if at_boundary {
            score += 8;
        }
        match prev_match {
            Some(p) if p + 1 == m => score += 5, // consecutive run
            Some(p) => score -= ((m - p - 1) as i32).min(3), // gap penalty, capped
            None => {}
        }
        prev_match = Some(m);
        hi = m + 1;
    }
    Some(score)
}

/// Indices into [`COMMANDS`] that match `query`, best match first. Ties keep
/// registry order (the sort is stable and the input is in registry order).
pub fn filter(query: &str) -> Vec<usize> {
    let mut scored: Vec<(usize, i32)> = COMMANDS
        .iter()
        .enumerate()
        .filter_map(|(i, c)| fuzzy_score(c.title, query).map(|s| (i, s)))
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1)); // stable: equal scores keep order
    scored.into_iter().map(|(i, _)| i).collect()
}

/// Indices into `choices` that match `query`, best match first — the same
/// scoring as [`filter`], but over a host-supplied list of strings (the
/// [`Mode::Choose`] picker). Ties keep the list's original order.
pub fn filter_choices(choices: &[String], query: &str) -> Vec<usize> {
    let mut scored: Vec<(usize, i32)> = choices
        .iter()
        .enumerate()
        .filter_map(|(i, s)| fuzzy_score(s, query).map(|sc| (i, sc)))
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1)); // stable: equal scores keep order
    scored.into_iter().map(|(i, _)| i).collect()
}

/// Whether the palette is browsing commands or collecting an argument for one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Typing into the search field, filtering the command list.
    Commands,
    /// A command needing a free-text argument was chosen; the input now collects
    /// that argument and Enter runs `action` with it.
    Argument {
        action: PaletteAction,
        prompt: &'static str,
    },
    /// A `choose` command was chosen and the host supplied its candidate list
    /// (see [`CommandPalette::enter_choose`]). The input now fuzzy-filters
    /// `choices`; Enter runs `action` with the highlighted entry. Unlike
    /// [`Mode::Argument`], the result is constrained to an existing entry.
    Choose {
        action: PaletteAction,
        prompt: &'static str,
    },
}

impl Default for Mode {
    fn default() -> Self {
        Mode::Commands
    }
}

/// The result of an Enter / Escape keystroke, handed back to `main.rs` so it
/// can run an action against `State` (which this module can't touch).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Nothing to do; stay open and redraw.
    Stay,
    /// Close the palette.
    Close,
    /// Run `action` (optionally with a typed argument), then close.
    Run {
        action: PaletteAction,
        arg: Option<String>,
    },
    /// A `choose` command was selected: the host must enumerate the candidate
    /// list for `action` and hand it back via [`CommandPalette::enter_choose`].
    /// The palette stays open. Kept separate from `Run` because only the host
    /// can read the choices (e.g. the schemes directory) — this module is pure.
    RequestChoices {
        action: PaletteAction,
        prompt: &'static str,
    },
}

/// The full palette state: open flag, the input field, current mode, the
/// filtered result list, and the selection/scroll cursor over it.
#[derive(Default)]
pub struct CommandPalette {
    pub open: bool,
    pub input: TextField,
    pub mode: Mode,
    /// In [`Mode::Commands`], indices into [`COMMANDS`]; in [`Mode::Choose`],
    /// indices into [`Self::choices`]. Empty in [`Mode::Argument`] (no list).
    pub filtered: Vec<usize>,
    /// The host-supplied candidate list backing [`Mode::Choose`]. Empty in every
    /// other mode. `filtered` indexes into this while choosing.
    pub choices: Vec<String>,
    /// Selected row within `filtered`.
    pub selected: usize,
    /// First visible row when `filtered` exceeds [`PALETTE_MAX_VISIBLE`].
    pub scroll: usize,
}

impl CommandPalette {
    /// Open fresh: empty query, full command list, nothing scrolled.
    pub fn open(&mut self) {
        self.open = true;
        self.mode = Mode::Commands;
        self.input.clear();
        self.selected = 0;
        self.scroll = 0;
        self.refilter();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.input.clear();
        self.mode = Mode::Commands;
        self.filtered.clear();
        self.choices.clear();
        self.selected = 0;
        self.scroll = 0;
    }

    pub fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.open();
        }
    }

    /// Drop into [`Mode::Choose`] over the host-supplied `choices`, fuzzy-filtered
    /// by what the user types. Called by the host in response to
    /// [`Outcome::RequestChoices`], once it has enumerated the candidates.
    pub fn enter_choose(
        &mut self,
        action: PaletteAction,
        prompt: &'static str,
        choices: Vec<String>,
    ) {
        self.mode = Mode::Choose { action, prompt };
        self.choices = choices;
        self.input.clear();
        self.selected = 0;
        self.scroll = 0;
        self.refilter();
    }

    /// Recompute `filtered` from the current query and keep the selection in
    /// range. Filters [`COMMANDS`] in command mode and [`Self::choices`] in
    /// choose mode. No-op (and leaves `filtered` empty) in argument mode, where
    /// there is no list.
    pub fn refilter(&mut self) {
        self.filtered = match self.mode {
            Mode::Commands => filter(&self.input.value),
            Mode::Choose { .. } => filter_choices(&self.choices, &self.input.value),
            Mode::Argument { .. } => return,
        };
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
        self.scroll = crate::completion::visible_window_start(
            self.selected,
            self.scroll.min(self.filtered.len()),
            PALETTE_MAX_VISIBLE,
        );
    }

    pub fn move_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = (self.selected + 1).min(self.filtered.len() - 1);
        self.scroll =
            crate::completion::visible_window_start(self.selected, self.scroll, PALETTE_MAX_VISIBLE);
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.scroll =
            crate::completion::visible_window_start(self.selected, self.scroll, PALETTE_MAX_VISIBLE);
    }

    /// The command currently highlighted, if any (only in commands mode).
    pub fn selected_command(&self) -> Option<&'static Command> {
        match self.mode {
            Mode::Commands => self.filtered.get(self.selected).map(|&i| &COMMANDS[i]),
            Mode::Argument { .. } | Mode::Choose { .. } => None,
        }
    }

    /// The label for the `i`th *visible* row (an index into `filtered`): a
    /// command title in command mode, a candidate string in choose mode. Used by
    /// the renderer so it doesn't need to know which list backs the current mode.
    pub fn row_label(&self, visible_index: usize) -> Option<&str> {
        let &idx = self.filtered.get(visible_index)?;
        match self.mode {
            Mode::Commands => Some(COMMANDS[idx].title),
            Mode::Choose { .. } => self.choices.get(idx).map(String::as_str),
            Mode::Argument { .. } => None,
        }
    }

    /// True when the current mode shows a scrollable result list (command or
    /// choose mode), as opposed to argument mode's bare input line.
    pub fn has_list(&self) -> bool {
        !matches!(self.mode, Mode::Argument { .. })
    }

    /// Handle Enter. In commands mode this runs an immediate command, enters
    /// argument mode (free-text commands), or emits [`Outcome::RequestChoices`]
    /// (pick-from-list commands). In argument mode it returns a `Run` carrying
    /// the typed text; in choose mode, a `Run` carrying the highlighted entry.
    pub fn accept(&mut self) -> Outcome {
        match self.mode {
            Mode::Commands => {
                let Some(cmd) = self.selected_command() else {
                    return Outcome::Stay; // empty list — nothing to run
                };
                match (cmd.arg_prompt, cmd.choose) {
                    (Some(prompt), true) => Outcome::RequestChoices {
                        action: cmd.action,
                        prompt,
                    },
                    (Some(prompt), false) => {
                        let action = cmd.action;
                        self.mode = Mode::Argument { action, prompt };
                        self.input.clear();
                        self.filtered.clear();
                        Outcome::Stay
                    }
                    (None, _) => Outcome::Run {
                        action: cmd.action,
                        arg: None,
                    },
                }
            }
            Mode::Argument { action, .. } => Outcome::Run {
                action,
                arg: Some(self.input.value.clone()),
            },
            Mode::Choose { action, .. } => match self.filtered.get(self.selected) {
                // Validation falls out of the closed list: only an existing
                // candidate can be selected. An empty/unmatched list runs nothing.
                Some(&i) => Outcome::Run {
                    action,
                    arg: Some(self.choices[i].clone()),
                },
                None => Outcome::Stay,
            },
        }
    }

    /// Handle Escape. From argument or choose mode it backs out to the command
    /// list; from the command list it closes the palette.
    pub fn escape(&mut self) -> Outcome {
        match self.mode {
            Mode::Argument { .. } | Mode::Choose { .. } => {
                self.mode = Mode::Commands;
                self.input.clear();
                self.choices.clear();
                self.selected = 0;
                self.scroll = 0;
                self.refilter();
                Outcome::Stay
            }
            Mode::Commands => Outcome::Close,
        }
    }

    /// Insert a typed character and refilter if we're searching.
    pub fn type_char(&mut self, c: char) {
        self.input.insert(c);
        self.refilter();
    }

    /// Backspace, refiltering if we're searching.
    pub fn backspace(&mut self) {
        self.input.backspace();
        self.refilter();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn textfield_inserts_and_moves() {
        let mut f = TextField::default();
        f.insert('a');
        f.insert('b');
        f.insert('c');
        assert_eq!(f.value, "abc");
        assert_eq!(f.cursor, 3);
        f.left();
        f.insert('X');
        assert_eq!(f.value, "abXc");
        assert_eq!(f.cursor_col(), 3);
    }

    #[test]
    fn textfield_backspace_and_delete() {
        let mut f = TextField {
            value: "hello".into(),
            cursor: 5,
        };
        f.backspace();
        assert_eq!(f.value, "hell");
        f.home();
        f.delete();
        assert_eq!(f.value, "ell");
        // Backspace at start and delete at end are no-ops.
        f.home();
        f.backspace();
        assert_eq!(f.value, "ell");
        f.end();
        f.delete();
        assert_eq!(f.value, "ell");
    }

    #[test]
    fn textfield_is_utf8_safe() {
        let mut f = TextField::default();
        f.insert('é');
        f.insert('λ');
        assert_eq!(f.value, "éλ");
        f.backspace();
        assert_eq!(f.value, "é");
        assert_eq!(f.cursor, "é".len());
    }

    #[test]
    fn fuzzy_matches_subsequence_only() {
        assert!(fuzzy_score("Set title", "stl").is_some());
        assert!(fuzzy_score("Set title", "set").is_some());
        assert!(fuzzy_score("Set title", "xyz").is_none());
        // Order matters for a subsequence.
        assert!(fuzzy_score("Set title", "lts").is_none());
    }

    #[test]
    fn fuzzy_empty_needle_matches() {
        assert_eq!(fuzzy_score("anything", ""), Some(0));
    }

    #[test]
    fn fuzzy_prefers_word_boundaries_and_runs() {
        // "st" as a consecutive prefix of a word should beat a scattered match.
        let boundary = fuzzy_score("Set title", "st").unwrap();
        let scattered = fuzzy_score("fastest", "st").unwrap();
        assert!(
            boundary > scattered,
            "boundary {boundary} should beat scattered {scattered}"
        );
    }

    #[test]
    fn filter_ranks_relevant_first() {
        let res = filter("title");
        // "Set title" and "Clear title" both contain "title"; both should be in
        // the results, and the list shouldn't be empty.
        assert!(!res.is_empty());
        let titles: Vec<&str> = res.iter().map(|&i| COMMANDS[i].title).collect();
        assert!(titles.contains(&"Set title"));
        assert!(titles.contains(&"Clear title"));
    }

    #[test]
    fn empty_query_returns_all_in_registry_order() {
        let res = filter("");
        assert_eq!(res.len(), COMMANDS.len());
        assert_eq!(res, (0..COMMANDS.len()).collect::<Vec<_>>());
    }

    #[test]
    fn accept_immediate_command_runs() {
        let mut p = CommandPalette::default();
        p.open();
        p.input.value = "reload".into();
        p.refilter();
        let out = p.accept();
        assert_eq!(
            out,
            Outcome::Run {
                action: PaletteAction::ReloadConfig,
                arg: None
            }
        );
    }

    #[test]
    fn accept_arg_command_enters_argument_mode_then_runs() {
        let mut p = CommandPalette::default();
        p.open();
        p.input.value = "set title".into();
        p.refilter();
        // First Enter: enters argument mode, stays open.
        assert_eq!(p.accept(), Outcome::Stay);
        assert!(matches!(p.mode, Mode::Argument { .. }));
        // Type the argument and submit.
        for c in "build server".chars() {
            p.type_char(c);
        }
        let out = p.accept();
        assert_eq!(
            out,
            Outcome::Run {
                action: PaletteAction::SetTitle,
                arg: Some("build server".into())
            }
        );
    }

    #[test]
    fn set_theme_command_is_registered_as_a_chooser() {
        let cmd = COMMANDS
            .iter()
            .find(|c| matches!(c.action, PaletteAction::SetTheme))
            .expect("Set theme command should be in COMMANDS");
        assert_eq!(cmd.title, "Set theme");
        // It picks from a host-supplied list, so it carries a prompt and the
        // `choose` flag rather than being a free-text or immediate command.
        assert_eq!(cmd.arg_prompt, Some("Theme"));
        assert!(cmd.choose);
    }

    #[test]
    fn filter_surfaces_set_theme() {
        for query in ["set theme", "theme"] {
            let res = filter(query);
            let titles: Vec<&str> = res.iter().map(|&i| COMMANDS[i].title).collect();
            assert!(
                titles.contains(&"Set theme"),
                "query {query:?} should surface \"Set theme\"; got {titles:?}"
            );
        }
    }

    #[test]
    fn light_and_dark_theme_commands_are_choosers() {
        for (action, title, prompt) in [
            (PaletteAction::SetLightTheme, "Set light theme", "Light theme"),
            (PaletteAction::SetDarkTheme, "Set dark theme", "Dark theme"),
        ] {
            let cmd = COMMANDS
                .iter()
                .find(|c| c.action == action)
                .expect("light/dark theme command should be registered");
            assert_eq!(cmd.title, title);
            assert_eq!(cmd.arg_prompt, Some(prompt));
            assert!(cmd.choose, "{title} should pick from the scheme list");
        }
    }

    #[test]
    fn toggle_follow_system_is_an_immediate_command() {
        let cmd = COMMANDS
            .iter()
            .find(|c| c.action == PaletteAction::ToggleFollowSystem)
            .expect("Toggle follow system command should be registered");
        // No prompt, not a chooser: selecting it runs straight away.
        assert_eq!(cmd.arg_prompt, None);
        assert!(!cmd.choose);
    }

    #[test]
    fn accept_toggle_follow_system_runs_immediately() {
        let mut p = CommandPalette::default();
        p.open();
        p.input.value = "follow system".into();
        p.refilter();
        assert_eq!(
            p.accept(),
            Outcome::Run {
                action: PaletteAction::ToggleFollowSystem,
                arg: None,
            }
        );
    }

    #[test]
    fn accept_set_dark_theme_requests_choices_with_its_prompt() {
        let mut p = CommandPalette::default();
        p.open();
        p.input.value = "set dark theme".into();
        p.refilter();
        assert_eq!(
            p.accept(),
            Outcome::RequestChoices {
                action: PaletteAction::SetDarkTheme,
                prompt: "Dark theme",
            }
        );
    }

    /// Selecting a `choose` command asks the host for candidates rather than
    /// running or entering free-text mode.
    #[test]
    fn accept_set_theme_requests_choices() {
        let mut p = CommandPalette::default();
        p.open();
        p.input.value = "set theme".into();
        p.refilter();
        assert_eq!(
            p.accept(),
            Outcome::RequestChoices {
                action: PaletteAction::SetTheme,
                prompt: "Theme",
            }
        );
        // The palette doesn't change mode on its own — it waits for the host to
        // supply the list via `enter_choose`.
        assert_eq!(p.mode, Mode::Commands);
    }

    /// The full picker flow: request choices, host installs them, the user
    /// fuzzy-filters and selects an existing entry, which runs with that value.
    #[test]
    fn enter_choose_then_filter_and_select_runs_with_chosen_value() {
        let mut p = CommandPalette::default();
        p.open();
        let choices = vec![
            "nostromo".to_string(),
            "spacedust".to_string(),
            "yutani".to_string(),
        ];
        p.enter_choose(PaletteAction::SetTheme, "Theme", choices);
        assert!(matches!(p.mode, Mode::Choose { .. }));
        // All candidates listed initially, in supplied order.
        assert_eq!(p.filtered.len(), 3);
        assert_eq!(p.row_label(0), Some("nostromo"));
        // Fuzzy-type to narrow to "spacedust".
        for c in "space".chars() {
            p.type_char(c);
        }
        assert_eq!(p.filtered.len(), 1);
        assert_eq!(p.row_label(0), Some("spacedust"));
        assert_eq!(
            p.accept(),
            Outcome::Run {
                action: PaletteAction::SetTheme,
                arg: Some("spacedust".into())
            }
        );
    }

    /// Validation: with no candidate matching the query (or an empty list), the
    /// picker has nothing to select, so Enter is a no-op rather than running with
    /// an invalid name.
    #[test]
    fn choose_with_no_match_runs_nothing() {
        let mut p = CommandPalette::default();
        p.open();
        p.enter_choose(
            PaletteAction::SetTheme,
            "Theme",
            vec!["nostromo".to_string(), "yutani".to_string()],
        );
        for c in "zzzz".chars() {
            p.type_char(c);
        }
        assert!(p.filtered.is_empty());
        assert_eq!(p.accept(), Outcome::Stay);

        // Likewise when the host supplied no candidates at all.
        p.enter_choose(PaletteAction::SetTheme, "Theme", Vec::new());
        assert!(p.filtered.is_empty());
        assert_eq!(p.accept(), Outcome::Stay);
    }

    /// Escape from the picker backs out to the command list (and clears the
    /// candidate list), mirroring argument mode.
    #[test]
    fn escape_backs_out_of_choose_then_closes() {
        let mut p = CommandPalette::default();
        p.open();
        p.enter_choose(PaletteAction::SetTheme, "Theme", vec!["yutani".to_string()]);
        assert!(matches!(p.mode, Mode::Choose { .. }));
        assert_eq!(p.escape(), Outcome::Stay);
        assert_eq!(p.mode, Mode::Commands);
        assert!(p.choices.is_empty());
        // Back in the command list, Escape now closes.
        assert_eq!(p.escape(), Outcome::Close);
    }

    #[test]
    fn escape_backs_out_of_argument_then_closes() {
        let mut p = CommandPalette::default();
        p.open();
        p.input.value = "set title".into();
        p.refilter();
        p.accept(); // -> argument mode
        assert!(matches!(p.mode, Mode::Argument { .. }));
        assert_eq!(p.escape(), Outcome::Stay); // back to commands
        assert_eq!(p.mode, Mode::Commands);
        assert_eq!(p.escape(), Outcome::Close); // now closes
    }

    #[test]
    fn navigation_clamps() {
        let mut p = CommandPalette::default();
        p.open(); // full list
        p.move_up(); // already at 0, stays
        assert_eq!(p.selected, 0);
        for _ in 0..100 {
            p.move_down();
        }
        assert_eq!(p.selected, p.filtered.len() - 1);
    }

    // ----- TextField: cursor clamping & multibyte edge cases -----

    #[test]
    fn textfield_left_clamps_at_zero_and_right_clamps_at_end() {
        let mut f = TextField {
            value: "ab".into(),
            cursor: 0,
        };
        // Left at the start is a no-op.
        f.left();
        assert_eq!(f.cursor, 0);
        // Walk right past the end; it should stop at value.len().
        f.right();
        f.right();
        f.right(); // one extra: must not overshoot
        assert_eq!(f.cursor, f.value.len());
        f.right();
        assert_eq!(f.cursor, f.value.len());
    }

    #[test]
    fn textfield_home_end_on_empty_string() {
        let mut f = TextField::default();
        f.home();
        assert_eq!(f.cursor, 0);
        f.end();
        assert_eq!(f.cursor, 0);
        // Delete and backspace on empty are no-ops.
        f.delete();
        f.backspace();
        assert_eq!(f.value, "");
        assert_eq!(f.cursor, 0);
    }

    #[test]
    fn textfield_delete_at_end_is_noop() {
        let mut f = TextField {
            value: "hi".into(),
            cursor: 2,
        };
        f.delete();
        assert_eq!(f.value, "hi");
        assert_eq!(f.cursor, 2);
    }

    #[test]
    fn textfield_insert_mid_then_delete_around_multibyte() {
        let mut f = TextField::default();
        // Build "aéb": insert 'a', 'é', then move home and forward to sit
        // between 'a' and 'é', then insert nothing — verify boundaries.
        for c in "aéb".chars() {
            f.insert(c);
        }
        assert_eq!(f.value, "aéb");
        assert_eq!(f.cursor, "aéb".len());
        // Move to just before 'é' (after 'a').
        f.home();
        f.right();
        assert_eq!(f.cursor, "a".len());
        assert_eq!(f.cursor_col(), 1);
        // Forward-delete removes the whole multibyte 'é', not a partial byte.
        f.delete();
        assert_eq!(f.value, "ab");
        assert_eq!(f.cursor, "a".len());
        // Now cursor sits before 'b'; backspace removes 'a'.
        f.backspace();
        assert_eq!(f.value, "b");
        assert_eq!(f.cursor, 0);
    }

    #[test]
    fn textfield_cursor_col_counts_chars_not_bytes() {
        let mut f = TextField::default();
        for c in "éλx".chars() {
            f.insert(c);
        }
        // Caret at end: three characters even though the byte length is larger.
        assert!(f.cursor > 3, "byte cursor should exceed char count");
        assert_eq!(f.cursor_col(), 3);
        // Move left twice: caret should report one column.
        f.left();
        f.left();
        assert_eq!(f.cursor_col(), 1);
    }

    // ----- fuzzy_score: case, single char, length, repeats -----

    #[test]
    fn fuzzy_is_case_insensitive_both_directions() {
        // Lowercase needle vs uppercase haystack and vice versa score equally.
        let a = fuzzy_score("SET TITLE", "set");
        let b = fuzzy_score("set title", "SET");
        assert!(a.is_some());
        assert_eq!(a, b);
    }

    #[test]
    fn fuzzy_single_char_needle_matches() {
        assert!(fuzzy_score("Zoom in", "z").is_some());
        assert!(fuzzy_score("Zoom in", "Z").is_some());
        assert!(fuzzy_score("Zoom in", "q").is_none());
    }

    #[test]
    fn fuzzy_needle_longer_than_haystack_fails() {
        assert!(fuzzy_score("hi", "hiya").is_none());
    }

    #[test]
    fn fuzzy_repeated_characters_consume_distinct_positions() {
        // "oo" must match the two adjacent o's; a single 'o' source can't satisfy
        // a two-'o' needle.
        assert!(fuzzy_score("Zoom", "oo").is_some());
        assert!(fuzzy_score("Zom", "oo").is_none());
    }

    #[test]
    fn fuzzy_consecutive_run_beats_gapped_match() {
        // Same matched characters, but one match is contiguous and the other has
        // gaps — contiguous should score at least as high.
        let contiguous = fuzzy_score("zoom", "zoo").unwrap();
        let gapped = fuzzy_score("zxoxo", "zoo").unwrap();
        assert!(
            contiguous > gapped,
            "contiguous {contiguous} should beat gapped {gapped}"
        );
    }

    // ----- filter: no-match and tie-break ordering -----

    #[test]
    fn filter_no_match_returns_empty() {
        // No command title is a superset of this subsequence.
        assert!(filter("qqqq").is_empty());
    }

    #[test]
    fn filter_ties_preserve_registry_order() {
        // "title" matches both "Set title" (index 0) and "Clear title" (index 1).
        // Whatever their relative scores, equal-scoring entries must keep their
        // registry order; check that the result is a subsequence of 0..len in
        // the case of an empty query (a guaranteed all-equal-score tie).
        let res = filter("");
        let mut sorted = res.clone();
        sorted.sort();
        assert_eq!(res, sorted, "equal scores must keep registry order");
    }

    // ----- CommandPalette: lifecycle, refilter clamping, arg-mode typing -----

    #[test]
    fn refilter_clamps_out_of_range_selection() {
        let mut p = CommandPalette::default();
        p.open();
        // Force selection past where a narrowed list will reach.
        p.selected = p.filtered.len() - 1;
        p.input.value = "zoom".into();
        p.refilter();
        assert!(!p.filtered.is_empty());
        assert!(
            p.selected < p.filtered.len(),
            "selected {} must be in range of {} results",
            p.selected,
            p.filtered.len()
        );
        // Narrow to nothing: selection clamps to 0 (saturating).
        p.input.value = "qqqq".into();
        p.refilter();
        assert!(p.filtered.is_empty());
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn open_resets_state() {
        let mut p = CommandPalette::default();
        // Dirty the state.
        p.input.value = "garbage".into();
        p.input.cursor = 3;
        p.selected = 4;
        p.scroll = 2;
        p.mode = Mode::Argument {
            action: PaletteAction::SetTitle,
            prompt: "Title",
        };
        p.open();
        assert!(p.open);
        assert_eq!(p.input.value, "");
        assert_eq!(p.input.cursor, 0);
        assert_eq!(p.selected, 0);
        assert_eq!(p.scroll, 0);
        assert_eq!(p.mode, Mode::Commands);
        assert_eq!(p.filtered.len(), COMMANDS.len());
    }

    #[test]
    fn close_resets_state() {
        let mut p = CommandPalette::default();
        p.open();
        p.input.value = "set".into();
        p.refilter();
        p.selected = 1;
        p.scroll = 1;
        p.close();
        assert!(!p.open);
        assert_eq!(p.input.value, "");
        assert_eq!(p.input.cursor, 0);
        assert_eq!(p.selected, 0);
        assert_eq!(p.scroll, 0);
        assert_eq!(p.mode, Mode::Commands);
        assert!(p.filtered.is_empty());
    }

    #[test]
    fn toggle_opens_then_closes() {
        let mut p = CommandPalette::default();
        assert!(!p.open);
        p.toggle();
        assert!(p.open);
        assert_eq!(p.filtered.len(), COMMANDS.len());
        p.toggle();
        assert!(!p.open);
        assert!(p.filtered.is_empty());
    }

    #[test]
    fn type_char_in_argument_mode_does_not_filter() {
        let mut p = CommandPalette::default();
        p.open();
        p.input.value = "set title".into();
        p.refilter();
        p.accept(); // -> argument mode; filtered cleared
        assert!(matches!(p.mode, Mode::Argument { .. }));
        assert!(p.filtered.is_empty());
        // Typing the argument must not repopulate the command list.
        for c in "abc".chars() {
            p.type_char(c);
        }
        assert_eq!(p.input.value, "abc");
        assert!(
            p.filtered.is_empty(),
            "refilter must be a no-op in argument mode"
        );
    }

    #[test]
    fn move_down_on_empty_filtered_is_noop() {
        let mut p = CommandPalette::default();
        p.open();
        p.input.value = "qqqq".into();
        p.refilter();
        assert!(p.filtered.is_empty());
        p.move_down();
        assert_eq!(p.selected, 0);
        assert_eq!(p.scroll, 0);
    }

    // ----- New window command registry entry -----

    #[test]
    fn new_window_command_is_registered() {
        // The "New window" command must exist in the registry and map to the
        // NewWindow action.
        let cmd = COMMANDS
            .iter()
            .find(|c| c.title == "New window")
            .expect("New window command should be registered");
        assert_eq!(cmd.action, PaletteAction::NewWindow);
    }

    #[test]
    fn new_window_runs_immediately_without_argument() {
        // No arg_prompt means selecting it runs right away rather than entering
        // argument mode.
        let cmd = COMMANDS
            .iter()
            .find(|c| c.action == PaletteAction::NewWindow)
            .unwrap();
        assert_eq!(cmd.arg_prompt, None);
    }

    #[test]
    fn new_window_is_discoverable_via_filter() {
        let new_window_idx = COMMANDS
            .iter()
            .position(|c| c.action == PaletteAction::NewWindow)
            .unwrap();
        // Both the full title and a prefix should surface the command.
        for query in ["new window", "new"] {
            let res = filter(query);
            assert!(
                res.contains(&new_window_idx),
                "query {query:?} should match the New window command"
            );
        }
    }

    #[test]
    fn scroll_offset_advances_past_max_visible() {
        // The static registry is shorter than PALETTE_MAX_VISIBLE, so drive the
        // scroll logic directly with a synthetic result list. Indices point at a
        // real command so the struct stays internally consistent.
        let mut p = CommandPalette::default();
        p.open = true;
        p.mode = Mode::Commands;
        p.filtered = vec![0; PALETTE_MAX_VISIBLE + 4];
        p.selected = 0;
        p.scroll = 0;
        // Walk past the visible window; scroll must begin advancing.
        for _ in 0..PALETTE_MAX_VISIBLE {
            p.move_down();
        }
        assert_eq!(p.selected, PALETTE_MAX_VISIBLE);
        assert!(
            p.scroll > 0,
            "scroll {} should advance once selection passes PALETTE_MAX_VISIBLE",
            p.scroll
        );
        // Selected stays within the visible window [scroll, scroll+max).
        assert!(p.selected >= p.scroll);
        assert!(p.selected < p.scroll + PALETTE_MAX_VISIBLE);
    }

    // ----- filter_choices: scoring over a host-supplied string list -----

    #[test]
    fn filter_choices_ranks_relevant_first() {
        let choices = vec![
            "nostromo".to_string(),
            "spacedust".to_string(),
            "yutani".to_string(),
        ];
        // A query that is a contiguous substring of exactly one entry should
        // surface that entry; a stronger (more boundary-aligned) match for a
        // query should outrank a scattered subsequence match. "ut" is a tight
        // run inside "yutani" but only a scattered subsequence of "nostromo"
        // ("...t..." after) — check the better match leads.
        let res = filter_choices(&choices, "yutani");
        assert!(!res.is_empty());
        assert_eq!(res[0], 2, "exact \"yutani\" should rank first");
    }

    #[test]
    fn filter_choices_empty_query_keeps_original_order() {
        let choices = vec![
            "alpha".to_string(),
            "bravo".to_string(),
            "charlie".to_string(),
        ];
        let res = filter_choices(&choices, "");
        assert_eq!(res, vec![0, 1, 2], "empty query keeps the supplied order");
    }

    #[test]
    fn filter_choices_no_match_returns_empty() {
        let choices = vec!["nostromo".to_string(), "yutani".to_string()];
        // No entry contains this subsequence.
        assert!(filter_choices(&choices, "zzzz").is_empty());
    }

    #[test]
    fn filter_choices_on_empty_list_is_empty() {
        // Both with and without a query, an empty candidate list yields nothing.
        assert!(filter_choices(&[], "").is_empty());
        assert!(filter_choices(&[], "anything").is_empty());
    }

    #[test]
    fn filter_choices_ties_preserve_list_order() {
        // An empty query scores every entry equally; the stable sort must leave
        // the result in ascending (original) index order.
        let choices = vec![
            "one".to_string(),
            "two".to_string(),
            "three".to_string(),
            "four".to_string(),
        ];
        let res = filter_choices(&choices, "");
        let mut sorted = res.clone();
        sorted.sort();
        assert_eq!(res, sorted, "equal scores must keep list order");
    }

    #[test]
    fn filter_choices_narrows_to_matching_subset() {
        let choices = vec![
            "nostromo".to_string(),
            "spacedust".to_string(),
            "yutani".to_string(),
        ];
        // "space" is a substring of exactly one entry.
        let res = filter_choices(&choices, "space");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0], 1);
    }

    // ----- row_label: per-mode label lookup over the filtered list -----

    #[test]
    fn row_label_returns_command_title_in_commands_mode() {
        let mut p = CommandPalette::default();
        p.open(); // Mode::Commands, full list in registry order.
        // The i-th visible row maps through `filtered` to a COMMANDS title.
        let first = p.filtered[0];
        assert_eq!(p.row_label(0), Some(COMMANDS[first].title));
    }

    #[test]
    fn row_label_returns_candidate_string_in_choose_mode() {
        let mut p = CommandPalette::default();
        p.open();
        p.enter_choose(
            PaletteAction::SetTheme,
            "Theme",
            vec![
                "nostromo".to_string(),
                "spacedust".to_string(),
                "yutani".to_string(),
            ],
        );
        // Unfiltered: visible rows mirror the supplied order.
        assert_eq!(p.row_label(0), Some("nostromo"));
        assert_eq!(p.row_label(1), Some("spacedust"));
        assert_eq!(p.row_label(2), Some("yutani"));
    }

    #[test]
    fn row_label_out_of_range_is_none() {
        let mut p = CommandPalette::default();
        p.open();
        // One past the last visible row has no label.
        assert_eq!(p.row_label(p.filtered.len()), None);
        assert_eq!(p.row_label(9999), None);
    }

    #[test]
    fn row_label_in_argument_mode_is_none() {
        let mut p = CommandPalette::default();
        p.open();
        p.input.value = "set title".into();
        p.refilter();
        p.accept(); // -> Mode::Argument, which clears `filtered`.
        assert!(matches!(p.mode, Mode::Argument { .. }));
        // No list backs argument mode, so every index is None.
        assert_eq!(p.row_label(0), None);
    }

    // ----- has_list: which modes show a scrollable result list -----

    #[test]
    fn has_list_true_in_commands_and_choose_false_in_argument() {
        let mut p = CommandPalette::default();
        p.open();
        assert!(p.has_list(), "commands mode shows a list");

        p.enter_choose(PaletteAction::SetTheme, "Theme", vec!["yutani".to_string()]);
        assert!(p.has_list(), "choose mode shows a list");

        p.input.clear();
        p.mode = Mode::Argument {
            action: PaletteAction::SetTitle,
            prompt: "Title",
        };
        assert!(!p.has_list(), "argument mode is a bare input line");
    }

    // ----- enter_choose: navigation clamping and scroll over candidates -----

    #[test]
    fn choose_navigation_clamps_within_candidates() {
        let mut p = CommandPalette::default();
        p.open();
        p.enter_choose(
            PaletteAction::SetTheme,
            "Theme",
            vec![
                "nostromo".to_string(),
                "spacedust".to_string(),
                "yutani".to_string(),
            ],
        );
        assert_eq!(p.selected, 0);
        // move_up at the top stays put.
        p.move_up();
        assert_eq!(p.selected, 0);
        // move_down clamps at the last candidate.
        for _ in 0..100 {
            p.move_down();
        }
        assert_eq!(p.selected, p.filtered.len() - 1);
        assert_eq!(p.selected, 2);
    }

    #[test]
    fn choose_scroll_advances_past_max_visible() {
        // Drive the scroll logic through a real Choose-mode candidate list that
        // is longer than the visible window (the static registry is too short).
        let mut p = CommandPalette::default();
        p.open();
        let choices: Vec<String> = (0..PALETTE_MAX_VISIBLE + 4)
            .map(|i| format!("theme{i}"))
            .collect();
        p.enter_choose(PaletteAction::SetTheme, "Theme", choices);
        // All candidates pass the empty-query filter, in order.
        assert_eq!(p.filtered.len(), PALETTE_MAX_VISIBLE + 4);
        assert_eq!(p.selected, 0);
        assert_eq!(p.scroll, 0);
        // Walk past the visible window; scroll must begin advancing.
        for _ in 0..PALETTE_MAX_VISIBLE {
            p.move_down();
        }
        assert_eq!(p.selected, PALETTE_MAX_VISIBLE);
        assert!(
            p.scroll > 0,
            "scroll {} should advance once selection passes PALETTE_MAX_VISIBLE",
            p.scroll
        );
        // Selected stays within the visible window [scroll, scroll+max).
        assert!(p.selected >= p.scroll);
        assert!(p.selected < p.scroll + PALETTE_MAX_VISIBLE);
        // And the highlighted row's label is still resolvable via row_label.
        assert_eq!(
            p.row_label(p.selected),
            Some(format!("theme{}", p.filtered[p.selected]).as_str())
        );
    }
}

use std::borrow::Cow;
use uuid::Uuid;

// `trim_start`, but return how many bytes were trimmed:
fn trim_start_offset(s: &str) -> (&str, usize) {
    let offset = s
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(idx, _)| idx)
        .unwrap_or(s.len());
    (s.get(offset..).unwrap(), offset)
}

// Copy of unstable split_at_checked, with unsafe `get_unchecked`
// replaced by `.get().unwrap()`:
pub fn split_at_checked(s: &str, mid: usize) -> Option<(&str, &str)> {
    // is_char_boundary checks that the index is in [0, .len()]
    if s.is_char_boundary(mid) {
        // SAFETY: just checked that `mid` is on a char boundary.
        Some((s.get(0..mid).unwrap(), s.get(mid..s.len()).unwrap()))
    } else {
        None
    }
}

#[derive(Debug)]
pub struct ParseInput<'a, C> {
    pub cmd: &'a str,
    pub offset: usize,
    pub ctx: &'a C,
    // In autocomplete mode, we only traverse into sub-parsers
    // when the current command is followed by a trailing space
    // character. This retains the current suggestions, useful for
    // commands with identical prefixes.
    pub autocomplete_mode: bool,
}

impl<'a, C> Clone for ParseInput<'a, C> {
    fn clone(&self) -> Self {
        ParseInput { ..*self }
    }
}

impl<'a, C> Copy for ParseInput<'a, C> {}

impl<'a, C> ParseInput<'a, C> {
    pub fn new(cmd: &'a str, ctx: &'a C, autocomplete_mode: bool) -> Self {
        ParseInput {
            cmd,
            offset: 0,
            ctx,
            autocomplete_mode,
        }
    }

    pub fn parse<M: Clone>(self) -> ParseChain<'a, C, M> {
        ParseChain::new(self)
    }

    pub fn map_ctx<T>(self, f: impl FnOnce(&'a C) -> &'a T) -> ParseInput<'a, T> {
        ParseInput {
            cmd: self.cmd,
            offset: self.offset,
            ctx: f(self.ctx),
            autocomplete_mode: self.autocomplete_mode,
        }
    }
}

#[derive(Debug)]
pub enum ArgumentParseResult<'a, C, M: Clone, T, E> {
    Ok(T, ParseChain<'a, C, M>),
    Missing(ParseChain<'a, C, M>),
    ParseError(E, ParseChain<'a, C, M>),
}

#[derive(Debug)]
pub struct ParseChain<'a, C, M: Clone> {
    pub input: ParseInput<'a, C>,
    pub exact_match: Option<M>,
    pub completions: Vec<String>,
    pub error: Option<Cow<'a, str>>,
}

impl<'a, C, M: Clone> Clone for ParseChain<'a, C, M> {
    fn clone(&self) -> Self {
        ParseChain {
            input: self.input.clone(),
            exact_match: self.exact_match.clone(),
            completions: self.completions.clone(),
            error: self.error.clone(),
        }
    }
}

impl<'a, C, M: Clone> ParseChain<'a, C, M> {
    pub fn new(input: ParseInput<'a, C>) -> Self {
        ParseChain {
            input,
            exact_match: None,
            completions: vec![],
            error: None,
        }
    }

    pub fn error<E: Into<Cow<'a, str>>>(self, msg: E) -> Self {
        ParseChain {
            error: Some(msg.into()),
            ..self
        }
    }

    pub fn missing_unknown_command_error(self) -> Self {
        if self.input.cmd.trim() == "" {
            self.error("Missing command")
        } else {
            self.error("Unknown command")
        }
    }

    pub fn parse_uuid_arg(mut self) -> ArgumentParseResult<'a, C, M, Uuid, uuid::Error> {
        // We retain the original `self`, in case of a parsing error:
        let original_self = self.clone();

        // Parse the following word as a UUID:
        let (_accepted_cmd, sub_cmd) = split_at_checked(self.input.cmd, self.input.offset).unwrap();
        let (sub_cmd, trimmed_offset) = trim_start_offset(sub_cmd);
        self.input.offset += trimmed_offset;

        match sub_cmd.split(' ').next() {
            None =>
            // Missing a Uuid argument:
            {
                ArgumentParseResult::Missing(original_self.error(format!("Missing UUID argument")))
            }

            Some(sub_cmd_arg) if sub_cmd_arg.trim() == "" =>
            // Missing a Uuid argument:
            {
                ArgumentParseResult::Missing(original_self.error(format!("Missing UUID argument")))
            }

            Some(sub_cmd_arg) => {
                // Try to parse `sub_cmd_arg` as a Uuid:
                match Uuid::parse_str(sub_cmd_arg) {
                    Ok(uuid) => {
                        // Is valid Uuid, add string length to offset:
                        self.input.offset += sub_cmd_arg.len();
                        ArgumentParseResult::Ok(uuid, self)
                    }
                    Err(e) => ArgumentParseResult::ParseError(
                        e,
                        original_self.error(format!("\"{}\" is not a valid UUID", sub_cmd_arg)),
                    ),
                }
            }
        }
    }

    pub fn parse_subcommands(
        mut self,
        available_subcommands: &[(
            &'static str,
            &dyn Fn(ParseInput<'a, C>) -> ParseChain<'a, C, M>,
        )],
    ) -> Self {
        let (accepted_cmd, sub_cmd) = split_at_checked(self.input.cmd, self.input.offset).unwrap();
        let (sub_cmd, trimmed_offset) = trim_start_offset(sub_cmd);
        self.input.offset += trimmed_offset;

        for (sc, sc_parser) in available_subcommands {
            // If the current sub_cmd starts with an exact match
            // for a subcommand, and, if in autocomplete mode,
            // that exact match is followed by a space character,
            // continue parsing at the subcommand:
            if sc.len() <= sub_cmd.len() {
                let (split_sc, split_tr) = sub_cmd.split_at(sc.len());
                if split_sc == *sc && (!self.input.autocomplete_mode || split_tr.starts_with(" ")) {
                    return sc_parser(ParseInput {
                        offset: self.input.offset + sc.len(),
                        ..self.input
                    });
                }
            }

            // Else, check if the subcommand starts with our current input. If
            // so, add it to the completions:
            if sc.starts_with(sub_cmd) {
                self.completions.push(format!(
                    "{}{}{}",
                    accepted_cmd.trim(),
                    if accepted_cmd.trim() == "" { "" } else { " " },
                    sc
                ));
            }
        }

        self
    }

    pub fn add_completions<T: std::fmt::Display + 'a, I: Iterator<Item = T> + 'a>(
        mut self,
        completions: impl FnOnce(&'a str) -> I,
    ) -> Self {
        let (accepted_cmd, sub_cmd) = split_at_checked(self.input.cmd, self.input.offset).unwrap();
        let (sub_cmd, _trimmed_offset) = trim_start_offset(sub_cmd);

        for c in completions(sub_cmd) {
            self.completions.push(format!(
                "{}{}{}",
                accepted_cmd.trim(),
                if accepted_cmd.trim() == "" { "" } else { " " },
                c
            ));
        }

        self
    }

    pub fn accept(self, m: M) -> Self {
        let (accepted_cmd, sub_cmd) = split_at_checked(self.input.cmd, self.input.offset).unwrap();

        if sub_cmd.trim() == "" {
            ParseChain {
                exact_match: Some(m),
                completions: vec![accepted_cmd.trim().to_string()],
                ..self
            }
        } else {
            self
        }
    }

    pub fn map_match<T: Clone>(self, f: impl FnOnce(M) -> T) -> ParseChain<'a, C, T> {
        ParseChain {
            input: self.input,
            exact_match: self.exact_match.map(move |m| f(m)),
            completions: self.completions,
            error: self.error,
        }
    }

    pub fn map_input_ctx<T: 'a>(self, f: impl FnOnce(&'a C) -> &'a T) -> ParseChain<'a, T, M> {
        ParseChain {
            input: self.input.map_ctx(f),
            exact_match: self.exact_match,
            completions: self.completions,
            error: self.error,
        }
    }
}

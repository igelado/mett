//! Finding addresses, as in mutt: aliases completed with Tab in the
//! address questions, queries (`Q`, and `^T` in those questions) through
//! `query_command` or the organisation's directory, and saving a message's
//! sender as an alias (`a`).

use mett_core::Address;

use super::composing::{Input, Question};
use super::{App, Effect, Screen, effect_id};
use crate::action::Action;
use crate::alias;
use crate::compose::Kind;
use crate::lineedit::LineEditor;
use crate::worker::{QuerySource, Request, RequestId};

/// A list of addresses to choose from: aliases, or a query's results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryMenu {
    /// What the list is, for the status bar.
    pub title: String,
    /// The entries.
    pub entries: Vec<QueryEntry>,
    /// The selected entry.
    pub selected: usize,
    /// The query on its way, if any.
    pub(super) request: Option<RequestId>,
    /// What choosing an entry does.
    pub(super) purpose: Purpose,
}

/// An entry of a [`QueryMenu`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryEntry {
    /// How it is shown.
    pub label: String,
    /// What choosing it puts in the question (an alias's name, or an
    /// address).
    pub insert: String,
    /// The address, for mailing it.
    pub address: Option<Address>,
}

/// What choosing an entry of a [`QueryMenu`] does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Purpose {
    /// Completes the address question it came from, whose text was `typed`
    /// before the cursor (with the entry being completed starting at
    /// `start`) and `rest` after it.
    Complete {
        question: Question,
        typed: String,
        start: usize,
        rest: String,
    },
    /// Writes a message to the address.
    Mail,
}

/// How to find addresses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuerySettings {
    /// `query_command`, if set.
    pub command: String,
    /// `office365_query_directory`: search the directory without one.
    pub directory: bool,
}

impl QuerySettings {
    /// Where queries go, if anywhere.
    fn source(&self) -> Option<QuerySource> {
        if !self.command.is_empty() {
            Some(QuerySource::Command(self.command.clone()))
        } else if self.directory {
            Some(QuerySource::Directory)
        } else {
            None
        }
    }
}

impl App {
    /// Whether `question` takes addresses.
    pub(super) fn takes_addresses(question: Question) -> bool {
        matches!(question, Question::To | Question::Cc | Question::Bcc)
    }

    /// Tab in an address question: completes the alias being typed, or
    /// lists the aliases it may be.
    pub(super) fn complete_alias(
        &mut self,
        question: Question,
        typed: String,
        rest: String,
    ) -> Vec<Effect> {
        let (start, entry) = alias::last_entry(&typed);
        let found = alias::completions(&self.aliases, entry);
        match found.as_slice() {
            [] => self.fail_at_question("No alias matches."),
            [only] => {
                let completed = format!("{}{}", &typed[..start], only.key);
                self.set_answer(&completed, alias::after_entry(&typed, &rest));
            }
            many => {
                let keys: Vec<&str> = many.iter().map(|a| a.key.as_str()).collect();
                let prefix = alias::common_prefix(&keys);
                // Further as far as they all go; then the list.
                if prefix.len() > entry.len() {
                    let completed = format!("{}{prefix}", &typed[..start]);
                    self.set_answer(&completed, alias::after_entry(&typed, &rest));
                    return Vec::new();
                }
                let entries = many
                    .iter()
                    .map(|a| QueryEntry {
                        label: format!("{:<15} {}", a.key, a.value),
                        insert: a.key.clone(),
                        address: None,
                    })
                    .collect();
                self.input = None;
                self.screens.push(Screen::Query(QueryMenu {
                    title: "Aliases".into(),
                    entries,
                    selected: 0,
                    request: None,
                    purpose: Purpose::Complete {
                        question,
                        typed,
                        start,
                        rest,
                    },
                }));
            }
        }
        Vec::new()
    }

    /// Puts `text`, then `rest`, in the question on the message line,
    /// with the cursor between them.
    fn set_answer(&mut self, text: &str, rest: &str) {
        if let Some(Input::Line { editor, .. }) = &mut self.input {
            *editor = LineEditor::with_cursor(text, rest);
        }
    }

    /// `^T` in an address question: queries for what is being typed.
    pub(super) fn query_for(
        &mut self,
        question: Question,
        typed: String,
        rest: String,
    ) -> Vec<Effect> {
        let (start, entry) = alias::last_entry(&typed);
        let entry = entry.to_owned();
        self.input = None;
        self.start_query(
            &entry,
            Purpose::Complete {
                question,
                typed,
                start,
                rest,
            },
        )
    }

    /// `Q`: asks what to query for.
    pub(super) fn ask_query(&mut self) {
        self.ask(Question::Query, "Query: ", "");
    }

    /// Queries for `text`, listing the results to `purpose`.
    pub(super) fn start_query(&mut self, text: &str, purpose: Purpose) -> Vec<Effect> {
        let text = text.trim();
        let Some(source) = self.query.source() else {
            let effects = self.back_to_question(&purpose);
            self.fail_at_question("Query command not defined.");
            return effects;
        };
        if text.is_empty() {
            return self.back_to_question(&purpose);
        }
        let request = self.request(Request::Query(source, text.to_owned()));
        self.screens.push(Screen::Query(QueryMenu {
            title: format!("Query '{text}'"),
            entries: Vec::new(),
            selected: 0,
            request: Some(effect_id(&request)),
            purpose,
        }));
        vec![request]
    }

    /// Back to the address question a list was for, with what was typed.
    fn back_to_question(&mut self, purpose: &Purpose) -> Vec<Effect> {
        if let Purpose::Complete {
            question,
            typed,
            rest,
            ..
        } = purpose
        {
            self.ask_line(*question, typed, rest);
        }
        Vec::new()
    }

    /// The request querying for the list on screen, while it is pending.
    pub(super) fn query_request(&self) -> Option<RequestId> {
        self.screens.iter().find_map(|s| match s {
            Screen::Query(menu) => menu.request,
            _ => None,
        })
    }

    /// Takes in a query's results.
    pub(super) fn on_queried(
        &mut self,
        id: RequestId,
        result: Result<Vec<Address>, String>,
    ) -> Vec<Effect> {
        let found = self
            .screens
            .iter_mut()
            .enumerate()
            .rev()
            .find_map(|(i, s)| match s {
                Screen::Query(menu) if menu.request == Some(id) => Some((i, menu)),
                _ => None,
            });
        let Some((position, menu)) = found else {
            return Vec::new();
        };
        menu.request = None;
        let failure = match result {
            Ok(found) if found.is_empty() => Some("No matches found.".to_owned()),
            Ok(found) => {
                menu.entries = found
                    .into_iter()
                    .map(|address| QueryEntry {
                        label: address.to_string(),
                        insert: Address::format_list(std::slice::from_ref(&address)),
                        address: Some(address),
                    })
                    .collect();
                None
            }
            Err(e) => Some(e),
        };
        if let Some(failure) = failure {
            let Screen::Query(menu) = self.screens.remove(position) else {
                unreachable!("found above");
            };
            let effects = self.back_to_question(&menu.purpose);
            self.fail_at_question(failure);
            return effects;
        }
        Vec::new()
    }

    pub(super) fn on_query_action(&mut self, action: Action) -> Vec<Effect> {
        let page = self.body_height.max(1);
        let Some(Screen::Query(menu)) = self.screens.last_mut() else {
            return Vec::new();
        };
        if super::navigate(&mut menu.selected, action, menu.entries.len(), page) {
            return Vec::new();
        }
        match action {
            Action::Exit | Action::Quit => {
                let Some(Screen::Query(menu)) = self.screens.pop() else {
                    unreachable!("checked above");
                };
                let mut effects = self.back_to_question(&menu.purpose);
                // A lookup still running is given up (its command killed).
                if let Some(query) = menu.request {
                    effects.push(self.request(Request::Cancel(query)));
                }
                return effects;
            }
            Action::DisplayMessage | Action::Mail => {
                let Some(entry) = menu.entries.get(menu.selected).cloned() else {
                    return Vec::new();
                };
                let Some(Screen::Query(menu)) = self.screens.pop() else {
                    unreachable!("checked above");
                };
                return match menu.purpose {
                    Purpose::Complete {
                        question,
                        typed,
                        start,
                        rest,
                    } => {
                        let separator = if start > 0 && !typed[..start].ends_with(' ') {
                            " "
                        } else {
                            ""
                        };
                        let text = format!("{}{separator}{}", &typed[..start], entry.insert);
                        // The rest of the entry goes; the entries after
                        // it stay.
                        let rest = alias::after_entry(&typed, &rest).to_owned();
                        self.ask_line(question, &text, &rest);
                        Vec::new()
                    }
                    Purpose::Mail => {
                        let to = entry.insert;
                        let effects = self.start_compose(Kind::New);
                        if matches!(
                            &self.input,
                            Some(Input::Line {
                                question: Question::To,
                                ..
                            })
                        ) {
                            self.set_answer(&to, "");
                        }
                        effects
                    }
                };
            }
            _ => {}
        }
        Vec::new()
    }

    /// `a`: saves the sender of the message acted on as an alias, asking
    /// for its name, then its address.
    pub(super) fn create_alias(&mut self) -> Vec<Effect> {
        // The pager's message, even if the index lost it meanwhile.
        let shown = match self.screens.last() {
            Some(Screen::Pager(pager)) => pager.message.as_ref().map(|m| &m.summary),
            _ => None,
        };
        let from = shown.and_then(|m| m.from.clone()).or_else(|| {
            self.acted_on()
                .and_then(|id| self.summary(&id))
                .and_then(|m| m.from.clone())
        });
        let Some(from) = from else {
            self.fail("No sender to make an alias of.");
            return Vec::new();
        };
        let name = from.name.as_deref().unwrap_or(&from.email);
        let key: String = name
            .split(|c: char| c.is_whitespace() || c == '@')
            .next()
            .unwrap_or_default()
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .collect::<String>()
            .to_lowercase();
        self.new_alias = Some(Address::format_list(&[from]));
        self.ask(Question::AliasName, "Alias as: ", &key);
        Vec::new()
    }

    /// The alias's name was given: asks for its address.
    pub(super) fn answer_alias_name(&mut self, key: String) -> Vec<Effect> {
        let address = self.new_alias.take().unwrap_or_default();
        let key = key.trim().to_owned();
        if key.is_empty() {
            return Vec::new();
        }
        if !alias::valid_key(&key) {
            self.fail(format!("{key:?} can't be an alias's name."));
            return Vec::new();
        }
        if alias::find(&self.aliases, &key).is_some() {
            self.fail("You already have an alias defined with that name!");
            return Vec::new();
        }
        self.alias_key = Some(key);
        self.ask(Question::AliasAddress, "Address: ", &address);
        Vec::new()
    }

    /// The alias's address was given: adds the alias, and saves it in
    /// `alias_file`.
    pub(super) fn answer_alias_address(&mut self, address: String) -> Vec<Effect> {
        let Some(key) = self.alias_key.take() else {
            return Vec::new();
        };
        let address = address.trim();
        // A line break would end the `alias` line, and start another
        // command.
        if address.contains(char::is_control) {
            self.fail("An alias's address can't have control characters.");
            return Vec::new();
        }
        // Saved as the addresses read, written out again: quoted where
        // they need to be, so the mettrc reads them back.
        let address = match Address::parse_list(address) {
            Ok(list) if list.is_empty() => {
                self.fail("No address: the alias wasn't added.");
                return Vec::new();
            }
            Ok(list) => Address::format_list(&list),
            Err(e) => {
                self.fail(format!("{e}"));
                return Vec::new();
            }
        };
        let line = alias::line(&key, &address);
        let mut effects = vec![Effect::Command(line.clone())];
        match &self.alias_file {
            Some(path) => effects.push(Effect::AppendFile {
                path: path.clone(),
                text: format!("{line}\n"),
            }),
            // Said once the command is done, which would clear it.
            None => {
                self.after_configured =
                    Some(format!("Alias {key} added (no alias_file to save it in)."));
            }
        }
        // The command it runs reports back like any `:` command.
        self.configuring += 1;
        effects
    }

    /// The alias was saved in `alias_file`, or not.
    pub(super) fn on_appended(
        &mut self,
        result: Result<std::path::PathBuf, String>,
    ) -> Vec<Effect> {
        let said = match result {
            Ok(path) => format!("Alias added to {}.", path.display()),
            Err(e) => format!("The alias couldn't be saved: {e}"),
        };
        // The `alias` command may still be running; it says so once done.
        if self.configuring > 0 {
            self.after_configured = Some(said);
        } else {
            self.status = said;
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests;

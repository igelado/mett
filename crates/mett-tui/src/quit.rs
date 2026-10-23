//! What the main loop does once the user has quit.

use crate::Incoming;
use crate::Slot;
use crate::UserEvent;
use crate::app;
use crate::app::Msg;
use crate::copy_all;
use crate::frontend::Frontend;
use crate::input::InputEvents;
use crate::rows;
use crate::show_tabs;
use crate::signin::SignInEvent;
use crate::worker;
use ratatui::crossterm::event::Event as TermEvent;
use ratatui::crossterm::event::KeyEventKind;
use tokio::sync::mpsc;

/// What a main loop that is quitting takes in of what an account says, and
/// how much longer to wait for it, if it should be: only what concerns
/// saving matters. Anything else (say, a late message fetch) could start new
/// work that would never be sent.
pub(crate) fn when_quitting(
    slot: &Slot,
    incoming: Incoming,
) -> Option<(Msg, Option<std::time::Duration>)> {
    let Incoming::Msg(msg) = incoming else {
        return None;
    };
    match msg {
        Msg::Response(response) => match &response.event {
            worker::Event::Throttled { wait, .. } if slot.app.saves(response.id) => {
                let wait = *wait + SAVE_TIMEOUT;
                Some((Msg::Response(response), Some(wait)))
            }
            worker::Event::FlagsUpdated(_)
            | worker::Event::Sent(..)
            | worker::Event::DraftSaved(..)
            | worker::Event::KeptDiscarded(_)
            | worker::Event::Transferred(_)
            | worker::Event::Purged(_)
            | worker::Event::Watched(_) => Some((Msg::Response(response), None)),
            _ => None,
        },
        // A sign-in that nothing left to save needs: the user has quit, so
        // don't send them to the browser.
        Msg::SignIn(SignInEvent::Prompt { .. }) if !slot.app.saving() => {
            if let Some(cancel) = &slot.cancel {
                cancel.cancel();
            }
            None
        }
        Msg::SignIn(event) => Some((Msg::SignIn(event), None)),
        // The countdown of a throttled save goes on; other timers would
        // start new work.
        Msg::Timer(timer @ app::Timer::Throttled(_)) => Some((Msg::Timer(timer), None)),
        _ => None,
    }
}

/// What the main loop is waiting for, once the user has quit: the changes
/// not yet saved, and watching stopped on the server.
pub(crate) async fn finish(
    screen: &mut impl Frontend,
    input_rx: &mut InputEvents,
    events_rx: &mut mpsc::UnboundedReceiver<(usize, Incoming)>,
    slots: &mut [Slot],
    active: usize,
    position: app::TabPosition,
) -> std::io::Result<()> {
    // Give pending changes (such as marking a message read) a moment to
    // reach the mailbox, rather than cancelling them by exiting.
    // The outcome is reported after the terminal is restored, so a change
    // that couldn't be saved isn't lost silently.
    // A save's sign-in failure may still be queued, behind its response
    // and the key that quit: take it in, so its window can offer a retry.
    while let Ok((at, incoming)) = events_rx.try_recv() {
        let slot = &mut slots[at];
        // What was saved meanwhile is saved: its response is taken in, or
        // the app would wait for it, and call it a failure.
        let Some((msg, _)) = when_quitting(slot, incoming) else {
            continue;
        };
        let effects = slot.app.update(msg);
        slot.carry_out(effects);
    }
    // Watching stops on the server too, while changes are saved.
    for slot in slots.iter_mut() {
        let effects = slot.app.unwatch();
        slot.carry_out(effects);
    }
    // The account with a sign-in window to use shows it.
    let mut shown = active;
    let unsaved = |slot: &Slot| !slot.app.save_failures().is_empty();
    let waiting = |slots: &[Slot]| {
        slots
            .iter()
            .any(|s| s.app.saving() || (unsaved(s) && s.app.sign_in.is_some()))
    };
    // Whether the user is signing in for changes that wait for it.
    let signing_in = |slots: &[Slot]| {
        slots
            .iter()
            .any(|s| s.app.sign_in.is_some() && (s.app.saving() || unsaved(s)))
    };
    if waiting(slots) {
        for slot in slots.iter_mut() {
            if slot.app.saving() {
                slot.app.status.info("Saving changes...");
            }
        }
        let mut stale = true;
        // A save the server throttled before the user quit waits that much
        // longer.
        let wait = |slots: &[Slot]| {
            slots
                .iter()
                .filter_map(|s| s.app.save_wait())
                .max()
                .unwrap_or_default()
        };
        let mut deadline = tokio::time::Instant::now() + SAVE_TIMEOUT + wait(slots);
        loop {
            // The window that takes the keys is the one of an account whose
            // changes wait for a sign-in (not one that is only signing in).
            if let Some(at) = slots
                .iter()
                .position(|s| s.app.sign_in.is_some() && (s.app.saving() || unsaved(s)))
            {
                shown = at;
            } else if let Some(at) = slots.iter().position(|s| s.app.saving()) {
                shown = at;
            }
            // A macro run in the sign-in window plays there.
            while let Some(msg) = slots[shown].app.next_sign_in_stroke() {
                let slot = &mut slots[shown];
                let effects = slot.app.update(msg);
                let copies = slot.carry_out(effects).copies;
                copy_all(screen, &mut slots[shown].app, copies);
                stale = true;
            }
            stale |= slots[shown].app.take_redraw();
            // A sign-in window keeps mett here only while a change is
            // still unsaved, so the user can sign in or retry it; other
            // sign-ins don't matter any more.
            if !waiting(slots) {
                break;
            }
            // What was ignored changed nothing to draw.
            if std::mem::take(&mut stale) {
                // The tab shown may be another's now.
                show_tabs(slots, shown, position);
                slots[shown].app.before_draw();
                screen.draw(&slots[shown].app)?;
            }
            let (at, msg) = tokio::select! {
                // Saving may need the user to sign in: the sign-in window
                // still works, and only it takes keys.
                event = input_rx.recv() => match event.transpose()? {
                    None => break,
                    Some(UserEvent::Term(TermEvent::Key(key)))
                        if key.kind != KeyEventKind::Release
                            && slots[shown].app.sign_in.is_some() =>
                    {
                        (shown, Msg::Key(key))
                    }
                    Some(UserEvent::Term(TermEvent::Resize(width, height))) => {
                        let tabbed = slots.len() > 1;
                        for slot in slots.iter_mut() {
                            slot.app.update(Msg::Resize {
                                width,
                                height: rows(height, tabbed),
                            });
                        }
                        stale = true;
                        continue;
                    }
                    // Closing the window again: no more waiting.
                    Some(UserEvent::Close) => break,
                    // Already quitting.
                    Some(_) => continue,
                },
                incoming = events_rx.recv() => match incoming {
                    Some((at, incoming)) => match when_quitting(&slots[at], incoming) {
                        Some((msg, wait)) => {
                            // A save the server throttled is tried again
                            // after the wait it asked for: wait for it that
                            // much longer.
                            if let Some(wait) = wait {
                                deadline = deadline.max(tokio::time::Instant::now() + wait);
                            }
                            (at, msg)
                        }
                        None => continue,
                    },
                    None => break,
                },
                // The user may take a while to sign in; the wait only
                // counts while they don't have to.
                () = tokio::time::sleep_until(deadline), if !signing_in(slots) => break,
            };
            let window_was_open = signing_in(slots);
            let slot = &mut slots[at];
            let effects = slot.app.update(msg);
            stale = true;
            // Nothing is edited, nor configured, any more; the sign-in
            // window may copy its link.
            let copies = slot.carry_out(effects).copies;
            copy_all(screen, &mut slots[at].app, copies);
            // The wait starts again once the user is done signing in.
            // (Still with what a throttled save has left to wait.)
            if window_was_open || signing_in(slots) {
                deadline = tokio::time::Instant::now() + SAVE_TIMEOUT + wait(slots);
            }
        }
    }
    // Not for long: what is left on the server expires anyway.
    let deadline = tokio::time::Instant::now() + UNWATCH_TIMEOUT;
    while slots.iter().any(|s| s.app.unwatching()) {
        tokio::select! {
            incoming = events_rx.recv() => match incoming {
                Some((at, Incoming::Msg(Msg::Response(response))))
                    if matches!(response.event, worker::Event::Watched(_)) =>
                {
                    let slot = &mut slots[at];
                    let effects = slot.app.update(Msg::Response(response));
                    slot.carry_out(effects);
                }
                Some(_) => {}
                None => break,
            },
            () = tokio::time::sleep_until(deadline) => break,
        }
    }
    if slots.iter().any(|s| s.app.saving()) {
        return Err(std::io::Error::other(
            "gave up waiting for changes to be saved; some messages may still be unread",
        ));
    }
    let failures: Vec<String> = slots
        .iter()
        .flat_map(|slot| {
            let prefix = failure_prefix(slot);
            slot.app
                .save_failures()
                .into_iter()
                .map(move |failure| format!("{prefix}{failure}"))
        })
        .collect();
    if !failures.is_empty() {
        return Err(std::io::Error::other(format!(
            "some changes could not be saved: {}",
            failures.join("; ")
        )));
    }
    Ok(())
}

/// What a saving failure of `slot`'s account starts with: its tab, if it
/// has one.
fn failure_prefix(slot: &Slot) -> String {
    match &slot.app.tabs {
        Some(_) => format!("{}: ", slot.label),
        None => String::new(),
    }
}

/// How long to wait for pending changes when quitting.
const SAVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long to wait, when quitting, for watching to stop on the server.
const UNWATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

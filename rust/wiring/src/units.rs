// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The units that start a session: LaunchAgent plists on macOS, systemd user
//! units on Linux. Both are read as plain text and matched by the same rules —
//! the difference between them is only where the evidence hides.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::{read_text, sorted_dir_names, Paths};

/// Session names shorter than this many bytes are not evidence.
///
/// Even matched on word boundaries, a name like `ai` fires inside a plist label
/// such as `ai.openclaw.gateway`, because a `.` *is* a boundary. Four is not a
/// measurement, it is the shortest length that stopped the false positives seen
/// in the field.
///
/// Counted in bytes: the collisions being avoided are with ASCII plist labels
/// and path segments, so a two-character name in a non-Latin script is six
/// bytes and passes — which is right, since nothing it could collide with is
/// written in that script.
const MIN_SESSION_LEN: usize = 4;

/// The pattern that decides "this unit text names that session".
///
/// Word-bounded: a bare substring search lets the session `ai` hit the
/// `ai-folder` inside an unrelated path, which is how an unrelated plist ended
/// up attributed to an agent. `None` when the name is too short to carry
/// information — see [`MIN_SESSION_LEN`].
pub(crate) fn session_needle(session: &str) -> Option<Regex> {
    if session.len() < MIN_SESSION_LEN {
        return None;
    }
    Some(word_bounded(session))
}

/// `needle` with a non-word character, or an end of the text, required on each
/// side.
///
/// The one definition of "as its own word" in this file: a session name, and
/// every verb in [`HERDR_SPAWN_VERBS`], are found by it. Two spellings of a
/// word boundary here would let `session_agents`'s two filters disagree about
/// what a word is.
fn word_bounded(needle: &str) -> Regex {
    Regex::new(&format!(
        r"(^|[^A-Za-z0-9_-]){}($|[^A-Za-z0-9_-])",
        regex::escape(needle)
    ))
    .expect("an escaped literal always compiles")
}

/// One launch unit: the label to report, and the text to match against.
pub(crate) struct UnitText {
    pub label: String,
    pub text: String,
}

/// What a launch unit is written in.
///
/// A comment is not a property of the bytes, it is a property of the grammar
/// they are written in: `<!-- -->` in a plist, a `#` or `;` line in a systemd
/// unit, a `#` line in the shell sidecar. One rule over all three cannot be
/// right for any of them — the rule that was tried, and withdrawn, is described
/// on [`xml_live_text`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Syntax {
    /// A LaunchAgent plist: XML.
    Plist,
    /// A systemd unit file.
    Unit,
    /// A `<stem>.up.sh` sidecar: a shell script.
    Shell,
}

/// A unit's text with the parts that are commented out taken away.
///
/// Every reader of a unit — the verbs [`spawns_session`] looks for, the session
/// name [`session_agents`] matches, the folders [`crate::collect`] matches, the
/// `.up.sh` stems [`with_sidecars`] follows — sees the same text, so a disabled
/// launcher is disabled for all of them. A plist keeps a retired launcher inside
/// a comment and an installer disables one by commenting the `ExecStart` line;
/// reading either as live names a unit that starts nothing as the session's
/// launcher, and `restore` then leaves that dead agent alone for good.
fn live_text(text: &str, syntax: Syntax) -> String {
    match syntax {
        Syntax::Plist => xml_live_text(text),
        // systemd: "Lines beginning with "#" or ";" are ignored" (systemd.syntax).
        Syntax::Unit => without_comment_lines(text, &['#', ';']),
        // A shell comment runs from `#` to the end of the line, and nothing
        // continues it.
        Syntax::Shell => without_comment_lines(text, &['#']),
    }
}

/// `text` without the lines that begin, after any blanks, with one of `marks`.
///
/// Whole lines only. A `#` further along a line starts a comment in the shell
/// too, but only outside quotes and parameter expansions, and deciding that
/// needs the shell's own reader — while the text this leaves can only hold
/// *extra* words, never fewer, so a launcher cannot go missing from it.
///
/// The line goes whole, newline and all, and nothing is welded by that: every
/// line that stays keeps its own newline, so the text either side of a comment
/// was already apart. (What does read across a newline is [`normalize_args`],
/// which turns any run of whitespace into one space — a limit of its own, and
/// the same on `main` with or without this pass.)
///
/// Known limit: a systemd comment ending in a backslash continues onto the next
/// line, which systemd itself deprecated and warns about, so what that next line
/// is depends on the version reading it. It is kept, which is the direction that
/// cannot lose a launcher: keeping a disabled one is the behaviour this function
/// found, while dropping a live one is what it must never introduce.
fn without_comment_lines(text: &str, marks: &[char]) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if marks.iter().any(|m| line.trim_start().starts_with(*m)) {
            continue;
        }
        out.push_str(line);
    }
    out
}

/// XML's own rule for where a comment ends, or the text untouched when the
/// markers do not follow it.
///
/// A comment is `<!--` up to the *first* `-->`, and inside a `<![CDATA[ … ]]>`
/// section the markers are data, not markup. Two things tell a comment from a
/// marker somebody wrote into an argument, and both come from the grammar rather
/// than from the bytes around it:
///
/// - **A comment does not start inside a text node.** Element text is what sits
///   between a `>` and the next `<`, and `<` is not allowed in it at all, so a
///   `<!--` with text of its own before it (`<string>open one with <!--`) is
///   data in that string. Whitespace before it is not text, which is where a
///   real comment sits: on its own line after `<array>`.
/// - **A comment does not close an element it did not open.** Its content is
///   outside the element tree, so a fragment starting `</string>` closed a
///   `<string>` that was opened *before* the marker — again data.
///
/// 🚨 When either fires, or a marker never closes, **nothing is taken away**.
/// That is the fix, not a detail. `(?s)<!--.*?-->` and a hand-bounded version of
/// it were both tried and withdrawn (PR #53): a lone `<!--` written into an
/// argument (`--prompt "open a block with <!-- like this"`) is not a comment, but
/// both read it as one and deleted everything to the next `-->` anywhere later in
/// the file.
///
/// Keeping the text is the answer whenever the grammar is unclear, because this
/// text is not read for launchers alone. [`crate::collect`] matches folders in
/// it and [`session_needle`] matches session names; a deletion can take a folder
/// or a name with it and mis-attribute wiring that has nothing to do with
/// starting anything. A block left in place can only be wrong about the launcher
/// verdict, and that verdict is printed with its reason and an override
/// (`restore <id>`, `rust/cli/src/restorecmd.rs:178`).
///
/// 🚨 `--` inside the content is *not* the test, although XML 1.0 §2.5 forbids
/// it. Requiring it was written and it refused the case this function exists for:
/// a person disables a launcher by commenting the block out, and the block says
/// `--label` or `--session`, so the commonest real comment carries `--` inside it.
/// The end-to-end test (`a_commented_out_launcher_is_not_the_launcher`) caught
/// that rule the moment it was written.
fn xml_live_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    // One pass, dispatching on what each `<!` opens: a second search of the
    // whole remainder per round would cost a scan per marker.
    while let Some(open) = text[at..].find("<!").map(|found| at + found) {
        let rest = &text[open..];
        if rest.starts_with("<![CDATA[") {
            // A section carries its markers as data, so it is copied whole.
            let Some(end) = rest.find("]]>") else {
                return text.to_string();
            };
            let stop = open + end + "]]>".len();
            out.push_str(&text[at..stop]);
            at = stop;
        } else if let Some(body) = rest.strip_prefix("<!--") {
            let Some(end) = body.find("-->") else {
                return text.to_string();
            };
            if inside_element_text(&text[..open]) || closes_what_it_did_not_open(&body[..end]) {
                return text.to_string();
            }
            out.push_str(&text[at..open]);
            at = open + "<!--".len() + end + "-->".len();
        } else {
            // A declaration (`<!DOCTYPE …`) is ordinary text to this.
            out.push_str(&text[at..open + "<!".len()]);
            at = open + "<!".len();
        }
    }
    out.push_str(&text[at..]);
    out
}

/// Whether `before` ends inside an element's text, which is where a `<!--` is
/// data rather than the start of a comment.
///
/// Text is what follows the last `>`. Whitespace does not count: a comment on
/// its own line is preceded by the newline and indent of the element it sits in.
fn inside_element_text(before: &str) -> bool {
    let tail = before.rfind('>').map_or(before, |at| &before[at + 1..]);
    !tail.trim().is_empty()
}

/// Whether `content` closes an element that was not opened inside it.
///
/// A comment's content sits outside the element tree, so it cannot close what it
/// did not open. One that does was never a comment: the marker was written
/// inside the element being closed. Opening tags left unclosed are allowed —
/// a block commented out in halves (`<!-- <array> -->` … ) is a comment, and the
/// closing half of that idiom is the one case this refuses to strip, which costs
/// nothing but a disabled launcher read as live.
fn closes_what_it_did_not_open(content: &str) -> bool {
    let mut open: Vec<&str> = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            return false;
        };
        let tag = &after[..end];
        rest = &after[end + 1..];
        if let Some(name) = tag.strip_prefix('/') {
            match open.pop() {
                Some(top) if top == name.trim() => {}
                _ => return true,
            }
        } else if !tag.starts_with('!') && !tag.starts_with('?') && !tag.ends_with('/') {
            open.push(tag.split_whitespace().next().unwrap_or(""));
        }
    }
    false
}

/// Pulls the stem out of the `<stem>.up.sh` a systemd `ExecStart` runs.
static UP_SH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"([^\s/"']+)\.up\.sh"#).expect("static pattern"));

/// Every launch unit, with the text it should be matched by.
///
/// A plist carries its command in its own body, so its text is the file.
/// systemd does not: an installer writes a unit whose `ExecStart` runs
/// `<session>.up.sh` and keeps the real start there, so the unit body alone
/// carries neither the folder nor the command that creates the place.
/// Concatenating the sidecar puts systemd on the same footing as a plist, so
/// one set of rules can judge both.
pub(crate) fn unit_texts(paths: &Paths) -> Vec<UnitText> {
    let mut out = Vec::new();

    if let Some(dir) = &paths.launch_agents_dir {
        for name in sorted_dir_names(dir) {
            let Some(label) = name.strip_suffix(".plist") else {
                continue;
            };
            let Some(text) = read_text(&dir.join(&name)) else {
                continue;
            };
            out.push(UnitText {
                label: label.to_string(),
                text: live_text(&text, Syntax::Plist),
            });
        }
    }

    if let Some(dir) = &paths.systemd_user_dir {
        for name in sorted_dir_names(dir) {
            let Some(label) = name.strip_suffix(".service") else {
                continue;
            };
            let Some(text) = read_text(&dir.join(&name)) else {
                continue;
            };
            // The unit's comments go before the stems are collected: a
            // commented-out `ExecStart` must not pull a retired sidecar in.
            out.push(UnitText {
                label: label.to_string(),
                text: with_sidecars(dir, live_text(&text, Syntax::Unit)),
            });
        }
    }
    out
}

/// Appends the `.up.sh` sidecars named by the unit body.
///
/// Stems are collected before anything is appended, so a sidecar that mentions
/// another `.up.sh` does not pull a further file in — the concatenation is one
/// level deep by design, and a set keeps a stem named twice (`ExecStart` and
/// `ExecStop`) from reading the same file twice.
fn with_sidecars(dir: &Path, mut text: String) -> String {
    let stems: BTreeSet<String> = UP_SH_RE
        .captures_iter(&text)
        .map(|c| c[1].to_string())
        .collect();
    for stem in stems {
        if let Some(s) = read_text(&dir.join(format!("{stem}.up.sh"))) {
            text.push('\n');
            // A shell script, so its comments are its own `#` lines — the unit
            // that named it has already had the unit's own comments taken out.
            text.push_str(&live_text(&s, Syntax::Shell));
        }
    }
    text
}

/// Labels of the units that start this session themselves.
///
/// Narrower than [`crate::collect`]'s `launch_units`, which also reports units
/// that merely mention the folder. restore uses this one to find the agents
/// launchd or systemd will revive on their own and leaves them alone. Taking a
/// card or monitor unit for the launcher is not cosmetic: restore creates the
/// session first, the real unit's start then fails as a duplicate, and the bot
/// never comes up at all.
///
/// A session name of under four bytes matches nothing, for the same
/// false-positive reason as in `collect`.
pub fn session_agents(paths: &Paths, session: &str) -> Vec<String> {
    let Some(re) = session_needle(session) else {
        return Vec::new();
    };
    unit_texts(paths)
        .into_iter()
        .filter(|u| spawns_session(&u.text) && re.is_match(&u.text))
        .map(|u| u.label)
        .collect()
}

/// The herdr commands that actually create a place (workspace, tab, agent,
/// named session). Only surfaces that exist in the herdr CLI belong here.
///
/// Subcommands only, each found anywhere in the text. 🚨 A flag does not belong
/// here, and `--session` was here: what a flag means depends on the program it
/// was written for, and this reads unit text rather than a parsed command line,
/// so it cannot say which program a word belongs to. `herdr --session work`
/// creates that place; `herdr-tower schedule add --session <id>` — the README's
/// own example — only names one that already exists, and the two are the same
/// text to a substring search.
///
/// A subcommand carries the program with it, so no attribution is needed: the
/// words `agent start` are not written next to each other by anything that is
/// not starting an agent.
///
/// Three of these come from a script that really did bring a bot up in herdr
/// (2026-09-11, `session_agents_herdr_real_script`): `workspace create`,
/// `tab create`, `agent start`. `session attach` is from the CLI surface, not
/// that script.
const HERDR_SPAWN_VERBS: [&str; 4] = [
    "workspace create",
    "tab create",
    "agent start",
    "session attach",
];

/// Argument markup and runs of whitespace, both flattened to a single space.
///
/// 🚨 A tag, and a tag holds no whitespace. `<[^>]*>` swallowed a shell
/// redirection and everything up to the next `>`: `exec < /dev/null` on one
/// line and `herdr tab create >> /tmp/log` on the next flattened to
/// ` exec > /tmp/log `, and the launcher command was gone from the text this
/// reads. `</?[A-Za-z][^>]*>` still did it for `exec </dev/null`, which opens
/// like a closing tag. A plist tag — `<string>`, `</string>` — never holds a
/// space, so requiring none ends the whole class. One that carries attributes
/// (`<plist version="1.0">`) is left as text, which only adds words no verb is
/// spelled with.
static ARG_SEP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"</?[A-Za-z][^>\s]*>|\s+").expect("static pattern"));

/// Flattens argument markup so a two-word command matches however it was split.
///
/// A LaunchAgent writes every argument as its own element, so the
/// `herdr tab create` that a shell script has on one line reads
/// `<string>tab</string><string>create</string>` in a plist — the substring
/// "tab create" does not occur in the file at all, and a space-joined match
/// misses a real launcher (found in review, 2026-09-11). Collapsing tags and
/// whitespace puts both spellings on one footing. The leading and trailing
/// space are a convenience for reading the result; the patterns that search it
/// carry their own end-of-text alternatives.
fn normalize_args(text: &str) -> String {
    let flat = ARG_SEP.replace_all(text, " ");
    format!(
        " {} ",
        flat.split_whitespace().collect::<Vec<_>>().join(" ")
    )
}

/// Whether a unit body starts a herdr place itself: `herdr` *plus* a command
/// that creates a place. The binary name alone is not enough — cards and
/// monitors mention `herdr` too, and calling one of those the launcher costs a
/// bot its start. Under systemd the command lives in the `<session>.up.sh`
/// sidecar, which [`unit_texts`] has already concatenated.
///
/// The verbs come from a script that really did bring a bot up in herdr
/// (2026-09-11, `session_agents_herdr_real_script`), not from reading help
/// output — if the CLI's surface moves, that test breaks first.
///
/// Known limit: a unit that only points at a script *path* carries no
/// evidence in its own text and is not found. systemd works around it with
/// the sidecar; a plist has to spell the command out.
///
/// Known limit: `herdr --session <name>` starts a place and is not found, since
/// only subcommands are evidence ([`HERDR_SPAWN_VERBS`]). That direction is the
/// safe one to be wrong in. Missing a launcher makes restore start the agent
/// itself, and the unit's own creation then fails as a duplicate — the agent is
/// up, started by restore rather than by the unit that was configured for it.
/// Naming the wrong unit as the launcher makes restore leave the dead agent out
/// altogether, crediting a unit that starts nothing. 🚨 Not silently, which an
/// earlier wording here claimed: the skip is printed with the unit's name and
/// the override that undoes it (`rust/cli/src/restorecmd.rs:178`). It is still
/// the worse of the two, since nothing brings that agent up unless somebody
/// reads the line and types the override.
///
/// Text that was commented out is not seen at all: [`live_text`] takes it away
/// by the grammar of the file it came from, before any of this runs. What
/// survives that is a plist whose comment markers do not follow XML's rule and a
/// systemd comment continued with a backslash, both of which are left whole on
/// purpose — see [`xml_live_text`] and [`without_comment_lines`].
pub(crate) fn spawns_session(text: &str) -> bool {
    if !text.contains("herdr") {
        return false;
    }
    let flat = normalize_args(text);
    SPAWN_VERB_RES.iter().any(|verb| verb.is_match(&flat))
}

/// Each verb of [`HERDR_SPAWN_VERBS`], word-bounded, built once.
///
/// 🚨 Not a bare substring, which is what this was: `herdr-tower card
/// myworkspace created` holds "workspace create" inside "myworkspace created",
/// and `herdr tab created a place` holds "tab create", so a unit that starts
/// nothing read as a launcher — and `restore` then skips a dead agent because
/// it believes a card's unit starts it.
///
/// 🚨 Nor a space on each side: a real launcher wears punctuation. The verb sits
/// inside `sh -c "herdr tab create"`, `$(herdr tab create)`, `…create; then`,
/// `…create&`, and `ARGS=agent start bot`. The same non-word class
/// [`session_needle`] uses admits all of those and still refuses the two above.
static SPAWN_VERB_RES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    HERDR_SPAWN_VERBS
        .iter()
        .copied()
        .map(word_bounded)
        .collect()
});

#[cfg(test)]
mod tests {
    use super::*;

    /// Naming a session is not starting one. A unit that only kills, watches or
    /// mentions a session must never be read as its launcher.
    #[test]
    fn only_place_creating_commands_count_as_a_launcher() {
        for (text, want) in [
            ("herdr tab create --label collab-bot", true),
            ("herdr workspace close collab-bot", false),
            ("herdr workspace create --label collab-bot", true),
            ("herdr agent start collab --kind claude --pane w1:p1", true),
            // 🚨 A flag is not evidence either way. This one does start a
            // place, and is missed; the next only names a place that exists,
            // and used to read as its launcher. Nothing in the text says which
            // program the flag was written for — see `HERDR_SPAWN_VERBS`.
            ("herdr --session collab-bot", false),
            ("herdr-tower schedule add +30m --session c376295e", false),
            ("herdr-tower card  # for herdr users", false),
            ("herdr-tower status", false),
        ] {
            assert_eq!(spawns_session(text), want, "{text:?}");
        }
    }

    /// 🚨 A verb inside a longer word is not that verb. `myworkspace created`
    /// held "workspace create" and `--session-id` held "--session", so a unit
    /// that only prints a card read as the launcher of a place — and restore
    /// skipped the dead agent whose real launcher it hid.
    ///
    /// The flag is no longer a verb at all ([`HERDR_SPAWN_VERBS`]), so its two
    /// spellings stay here as the guard against putting it back: whoever does
    /// has to make them pass again.
    #[test]
    fn a_verb_inside_a_longer_word_is_not_a_launcher() {
        for text in [
            "herdr-tower card myworkspace created",
            "herdr --session-id abc123",
            "herdr --session-file /tmp/x",
            "herdr tab created a place",
            "herdr agent started",
            "herdr session attached",
            "herdr xworkspace create",
            "herdr workspace createx",
        ] {
            assert!(!spawns_session(text), "{text:?}");
        }
    }

    /// 🚨 And a verb wearing punctuation still is one. A launcher is written
    /// inside `sh -c "…"`, inside `$( )`, before a `;` or an `&`, after an `=`,
    /// or split across plist elements. A rule that wanted a space on each side
    /// missed every one of these, and restore then started a session the unit
    /// was about to start too.
    #[test]
    fn a_verb_wearing_punctuation_is_still_a_launcher() {
        for text in [
            r#"ExecStart=/bin/sh -c "herdr tab create --label collab-bot""#,
            "PANE=$(herdr tab create)",
            "if herdr tab create; then echo up; fi",
            "herdr tab create&",
            // A verb right after an `=`, which is where a unit puts its
            // arguments.
            r#"Environment="ARGS=agent start bot" ExecStart=/usr/bin/herdr"#,
            "<string>herdr</string><string>tab</string><string>create</string>",
            r#"<string>-c</string><string>"herdr workspace create"</string>"#,
            // A redirection is not markup: the command must survive
            // flattening, with or without a space after the `<`.
            "exec < /dev/null\nherdr tab create --label x >> /tmp/log 2>&1",
            "exec </dev/null\nherdr tab create --label x >> /tmp/log 2>&1",
            "ExecStart=/bin/sh -c 'cat <in.txt; herdr tab create -l collab-bot >out'",
            "<string>/bin/sh</string><string>-c</string><string>exec </dev/null; herdr tab create -l x</string>",
        ] {
            assert!(spawns_session(text), "{text:?}");
        }
    }

    /// Every verb has to be spelled the way flattened text spells one: single
    /// spaces, none at the ends. A stray space in the const could never match,
    /// and both tables above would still pass while `restore` quietly stopped
    /// recognising that verb.
    #[test]
    fn every_verb_can_be_found_in_a_unit_that_says_it() {
        for verb in HERDR_SPAWN_VERBS {
            let unit = format!("ExecStart=/usr/local/bin/herdr {verb} collab-bot");
            assert!(spawns_session(&unit), "{verb:?}");
        }
    }

    /// Cross validation over generated units: the search agrees with a plain
    /// reading — one that never calls the flattening it is checking, but builds
    /// the text it expects from the words and gaps it chose. The vocabulary
    /// trips both mistakes: every verb's words also appear inside longer ones,
    /// and the gaps are the punctuation a real unit wears. A plist of the same
    /// words must read the same as the line, which is the whole reason the
    /// flattening exists.
    #[test]
    fn a_verb_is_read_the_same_way_a_person_reads_the_words() {
        const WORDS: [&str; 18] = [
            "herdr",
            "herdr-tower",
            "workspace",
            "myworkspace",
            "create",
            "created",
            "createx",
            "tab",
            "tabs",
            "agent",
            "start",
            "started",
            "session",
            "attach",
            "attached",
            "--session",
            "--session-id",
            "--session-file",
        ];
        const GAPS: [&str; 10] = [" ", "  ", "\t", "=", "\"", "$(", ")", ";", "&", "\n"];
        // The rule said plainly, over text this test built rather than text the
        // code flattened: the verb's own characters, with a non-word character
        // or an end on each side.
        let plainly = |text: &str| {
            let wordish = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
            HERDR_SPAWN_VERBS.iter().any(|verb| {
                text.match_indices(verb).any(|(at, _)| {
                    let before = text[..at].chars().next_back();
                    let after = text[at + verb.len()..].chars().next();
                    before.is_none_or(|c| !wordish(c)) && after.is_none_or(|c| !wordish(c))
                })
            })
        };
        let mut rng = tower_testrng::Rng::new(0x5e55_1014);
        let mut launchers = 0;
        for round in 0..500 {
            // (word, the gap that follows it). A third of the rounds say a verb
            // outright: two words of a verb never land next to each other often
            // enough by chance, and a run with no launcher in it proves nothing.
            let mut pieces: Vec<(&str, &str)> = (0..1 + rng.below(6))
                .map(|_| (WORDS[rng.below(WORDS.len())], GAPS[rng.below(GAPS.len())]))
                .collect();
            if rng.below(3) == 0 {
                let verb = HERDR_SPAWN_VERBS[rng.below(HERDR_SPAWN_VERBS.len())];
                let at = rng.below(pieces.len() + 1);
                let mut said: Vec<(&str, &str)> = verb.split(' ').map(|w| (w, " ")).collect();
                if let Some(last) = said.last_mut() {
                    last.1 = GAPS[rng.below(GAPS.len())];
                }
                for (i, piece) in said.into_iter().enumerate() {
                    pieces.insert(at + i, piece);
                }
            }

            let mut line = String::new();
            let mut expected = String::from(" ");
            for (i, (word, gap)) in pieces.iter().enumerate() {
                line.push_str(word);
                expected.push_str(word);
                if i + 1 < pieces.len() {
                    line.push_str(gap);
                    // Whitespace becomes one space; anything else joins the words.
                    expected.push_str(if gap.trim().is_empty() { " " } else { gap });
                }
            }
            expected.push(' ');

            let want = line.contains("herdr") && plainly(&expected);
            launchers += usize::from(want);
            assert_eq!(spawns_session(&line), want, "round {round}: {line:?}");

            // The same words as a plist, one element each: the markup goes, so
            // the words stand alone and the answer is the joined-by-spaces one.
            let words: Vec<&str> = pieces.iter().map(|(word, _)| *word).collect();
            let plist: String = words
                .iter()
                .map(|w| format!("<string>{w}</string>"))
                .collect();
            let spaced = format!(" {} ", words.join(" "));
            assert_eq!(
                spawns_session(&plist),
                plist.contains("herdr") && plainly(&spaced),
                "round {round}: {plist:?}"
            );
            assert_eq!(
                spawns_session(&plist),
                spawns_session(&words.join(" ")),
                "round {round}: a plist reads as its line: {plist:?}"
            );
        }
        // Both answers have to occur, or the run proves nothing. Most gaps are
        // punctuation that separates the words of a two-word verb, so launchers
        // are the minority.
        assert!(
            (10..490).contains(&launchers),
            "{launchers} of 500 rounds were launchers — the vocabulary stopped mixing"
        );
    }

    /// Word boundaries alone do not save a two-letter name — a plist label is
    /// full of boundaries.
    #[test]
    fn short_session_names_produce_no_needle() {
        assert!(session_needle("ai").is_none());
        assert!(session_needle("t8").is_none());
        assert!(session_needle("collab-bot").is_some());
    }

    /// Regex metacharacters in a session name are matched literally; without
    /// escaping, a name like `a.b` would also match `axb`.
    #[test]
    fn session_names_are_matched_literally() {
        let re = session_needle("a.b-c").unwrap();
        assert!(re.is_match("herdr tab create --label a.b-c"));
        assert!(!re.is_match("herdr tab create --label axb-c"));
    }

    /// tmux is not handled by this tool any more (2026-09-22). A unit that only
    /// starts a tmux session revives nothing restore can see, and reading it as
    /// a launcher is the expensive mistake: restore would leave a dead agent
    /// alone, waiting for a unit that brings back something else entirely.
    ///
    /// Whether the same words split a different way — one plist element per
    /// argument against one shell line — reach the same verdict is generated
    /// beside [`normalize_args`], where the separators are the real punctuation
    /// a plist puts between them.
    #[test]
    fn a_unit_that_only_starts_tmux_is_not_a_launcher() {
        for (text, want) in [
            ("tmux new-session -s collab-bot", false),
            ("tmux kill-session -t collab-bot", false),
            (
                "<string>/usr/local/bin/tmux</string><string>new-session</string>\
                 <string>-s</string><string>collab-bot</string>",
                false,
            ),
            // herdr is what counts; tmux standing beside it changes nothing.
            (
                "tmux new-session -s collab-bot; herdr tab create --label collab-bot",
                true,
            ),
            ("", false),
        ] {
            assert_eq!(spawns_session(text), want, "{text:?}");
        }
    }

    /// A systemd unit and the files the installer writes beside it.
    fn unit_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        dir
    }

    /// `.up.sh` is the only sidecar now. The `.tmux-cmd` file the old
    /// installers wrote beside a unit is no longer read, so a unit whose only
    /// evidence lived there is no longer a launcher — and one whose `.up.sh`
    /// holds the real start still is.
    #[test]
    fn only_the_up_sh_sidecar_is_appended() {
        let dir = unit_dir(&[
            (
                "collab-bot.up.sh",
                "#!/bin/bash\nexec herdr workspace create --label collab-bot --cwd /w/app\n",
            ),
            ("collab-bot.tmux-cmd", "tmux new-session -s collab-bot\n"),
        ]);
        let unit = "[Service]\nExecStart=/bin/bash /x/collab-bot.up.sh\n";
        let text = with_sidecars(dir.path(), unit.to_string());

        assert!(text.contains("herdr workspace create"), "{text:?}");
        assert!(
            !text.contains("new-session"),
            "the .tmux-cmd sidecar was read: {text:?}"
        );
        assert!(spawns_session(&text), "{text:?}");

        // The same unit with only the `.tmux-cmd` beside it carries nothing.
        let tmux_only = unit_dir(&[("collab-bot.tmux-cmd", "tmux new-session -s collab-bot\n")]);
        let text = with_sidecars(tmux_only.path(), unit.to_string());
        assert_eq!(text, unit, "a file that is not a .up.sh must not be read");
        assert!(!spawns_session(&text));
    }

    /// A stem named by a unit that has no `.up.sh` beside it — the ordinary
    /// macOS-style case, where the command is spelled out in the unit itself —
    /// leaves the text exactly as it was, rather than losing it or failing.
    #[test]
    fn a_sidecar_that_is_not_there_changes_nothing() {
        let dir = unit_dir(&[]);
        let unit = "[Service]\nExecStart=/bin/bash /x/collab-bot.up.sh\n";
        assert_eq!(with_sidecars(dir.path(), unit.to_string()), unit);
        assert_eq!(with_sidecars(dir.path(), String::new()), "");
    }

    /// The concatenation is one level deep and each stem is read once. A
    /// sidecar naming a further `.up.sh` must not pull that file in, and a stem
    /// named by both `ExecStart` and `ExecStop` must not be appended twice —
    /// the text is evidence, and duplicated evidence is a unit reported under
    /// two names' worth of matches for no reason.
    #[test]
    fn sidecars_are_read_once_and_never_chained() {
        let dir = unit_dir(&[
            (
                "collab-bot.up.sh",
                "herdr workspace create --label collab-bot\nsource /x/deeper.up.sh\n",
            ),
            ("deeper.up.sh", "DEEPER_MARKER=1\n"),
        ]);
        let unit = "[Service]\n\
                    ExecStart=/bin/bash /x/collab-bot.up.sh\n\
                    ExecStop=/bin/bash /x/collab-bot.up.sh\n";
        let text = with_sidecars(dir.path(), unit.to_string());

        assert_eq!(
            text.matches("herdr workspace create").count(),
            1,
            "a stem named twice was read twice: {text:?}"
        );
        assert!(
            !text.contains("DEEPER_MARKER"),
            "a sidecar pulled in a second one: {text:?}"
        );
    }

    /// A unit written in another script survives the concatenation intact, and
    /// the sidecar it names is still found: the stem stops at whitespace and
    /// path separators, not at ASCII. A name of two Korean syllables is ten
    /// bytes, so it also passes the length floor that a two-letter ASCII name
    /// does not.
    #[test]
    fn a_unit_and_its_sidecar_keep_their_unicode() {
        let dir = unit_dir(&[(
            "세션-봇.up.sh",
            "herdr workspace create --label 세션-봇 --cwd /w/앱  # 자동 시작 🗼\n",
        )]);
        let unit = "[Unit]\n\
                    Description=세션 봇 🗼\n\
                    [Service]\n\
                    ExecStart=/bin/bash /x/세션-봇.up.sh\n";
        let text = with_sidecars(dir.path(), unit.to_string());

        assert!(text.contains("Description=세션 봇 🗼"), "{text:?}");
        assert!(text.contains("자동 시작 🗼"), "{text:?}");
        assert!(spawns_session(&text), "{text:?}");
        assert!(
            session_needle("세션-봇").unwrap().is_match(&text),
            "{text:?}"
        );
    }
    /// A comment is whatever the grammar of the file says it is, and nothing
    /// else. 🚨 One rule over all three kinds is what this replaces: a plist's
    /// `<!-- -->` means nothing in a systemd unit, which comments with `#` and
    /// `;`, and a plist's `#` is an ordinary character in an argument.
    #[test]
    fn a_comment_is_read_by_the_grammar_of_the_file_it_is_in() {
        for (syntax, text, want) in [
            // A plist keeps a retired launcher in a comment; the live one
            // beside it still counts, and XML joins the text a comment sat in.
            (
                Syntax::Plist,
                "<!-- herdr agent start bot --><string>herdr-tower</string>",
                false,
            ),
            (
                Syntax::Plist,
                "<!--a--><string>herdr</string><string>agent</string><string>start</string><!--b-->",
                true,
            ),
            // 🚨 A marker with text of its own before it, in the same node, is
            // data — so this is left whole and reads as no launcher. XML would
            // join `work` to `space` around a comment there, but that shape is
            // also exactly how a hand-written marker lands in an argument, and
            // a plist argument is character data, not mixed content. Being
            // wrong this way costs a printed skip reason; the other way deletes
            // live text that `collect` and `session_needle` also read.
            (
                Syntax::Plist,
                "<string>herdr work<!--legacy-->space create</string>",
                false,
            ),
            // `#` is not a comment in XML: it is a character in an argument.
            (
                Syntax::Plist,
                "<string>herdr</string><string>agent</string><string>start</string><string>#bot</string>",
                true,
            ),
            // systemd: `#` and `;`, blanks before them allowed, and a `#`
            // further along a live line is left alone — extra words only.
            (
                Syntax::Unit,
                "[Service]\n# ExecStart=/usr/bin/herdr agent start bot\nExecStart=/usr/bin/herdr-tower card\n",
                false,
            ),
            (
                Syntax::Unit,
                "[Service]\n   ; ExecStart=/usr/bin/herdr agent start bot\n",
                false,
            ),
            (
                Syntax::Unit,
                "[Service]\nExecStart=/usr/bin/herdr agent start bot  # the real one\n",
                true,
            ),
            // 🚨 An XML comment in a systemd unit is not a comment. The
            // withdrawn test claimed it was; systemd would refuse the line
            // outright, and this code cannot invent a rule systemd does not
            // have.
            (
                Syntax::Unit,
                "[Service]\n<!-- ExecStart=/usr/bin/herdr agent start bot -->\n",
                true,
            ),
            // A shell sidecar: its own `#` lines, shebang included.
            (
                Syntax::Shell,
                "#!/bin/bash\n# exec herdr agent start bot\nexec herdr-tower card\n",
                false,
            ),
            (
                Syntax::Shell,
                "exec herdr agent start bot  # still the launcher\n",
                true,
            ),
        ] {
            let live = live_text(text, syntax);
            assert_eq!(spawns_session(&live), want, "{syntax:?} {text:?} -> {live:?}");
        }
    }

    /// A comment goes as a whole line and takes nothing with it: the lines
    /// around it keep their own newlines, so they are no closer than they were.
    ///
    /// 🚨 That is not the same as the verb being refused. [`normalize_args`]
    /// turns any run of whitespace into one space, so `…herdr agent` on one line
    /// and `start bot` on the next read as one run here and on `main` alike —
    /// a limit of the flattening, which this pass neither causes nor cures.
    #[test]
    fn a_comment_goes_as_a_whole_line_and_takes_nothing_with_it() {
        for (text, want) in [
            (
                "ExecStart=/usr/bin/herdr agent\n# note\nstart bot\n",
                "ExecStart=/usr/bin/herdr agent\nstart bot\n",
            ),
            // A comment as the last line, with no newline of its own.
            ("live\n# gone", "live\n"),
            // Nothing but comments, and nothing at all.
            ("# a\n; b\n", ""),
            ("", ""),
        ] {
            assert_eq!(live_text(text, Syntax::Unit), want, "{text:?}");
        }
    }

    /// 🚨 A marker that is data leaves the text whole, launcher included.
    ///
    /// This is the case that withdrew two earlier attempts (PR #53): a lone
    /// `<!--` written into an argument is not a comment, and reading it as one
    /// deletes everything up to the next `-->` anywhere later in the file — the
    /// live launcher in between with it.
    #[test]
    fn a_plist_marker_that_is_data_keeps_every_byte() {
        let launcher = "<string>herdr</string><string>agent</string><string>start</string>";
        for text in [
            // A stray marker in an argument, a real comment later in the file,
            // and the launcher in between: the marker has text before it in its
            // own node.
            format!(
                "<string>--prompt</string><string>open one with <!-- like this</string>{launcher}<!-- retired -->"
            ),
            // The marker opens and never closes.
            format!("<string>open one with <!-- like this</string>{launcher}"),
            // A marker at the very start of a node, so nothing precedes it
            // there — caught instead by the `</string>` it would close without
            // having opened it.
            format!("<string><!-- weird</string>{launcher}<!-- retired -->"),
            // An unclosed CDATA section, so where its data ends is unknown.
            format!("<![CDATA[ herdr agent start bot {launcher}"),
        ] {
            let live = live_text(&text, Syntax::Plist);
            assert_eq!(live, text, "text was edited anyway");
            assert!(spawns_session(&live), "the launcher was lost: {live:?}");
        }
    }

    /// A comment a person wrote goes, whatever is inside it. 🚨 Requiring the
    /// content to be free of `--` (XML 1.0 §2.5 forbids it there) was written
    /// first and refused the case this exists for: a disabled launcher block
    /// says `--label` or `--session`, so the commonest real comment carries `--`.
    #[test]
    fn a_comment_goes_even_when_its_content_breaks_the_rules() {
        let launcher = "<string>herdr</string><string>agent</string><string>start</string>";
        for (text, want) in [
            // The dashes of a flag inside a disabled block.
            (
                format!("<!--<string>herdr</string><string>workspace</string><string>create</string><string>--label</string>-->{launcher}"),
                launcher.to_string(),
            ),
            // Prose with a dash run in it.
            (
                format!("<!-- retired -- for now -->{launcher}"),
                launcher.to_string(),
            ),
            // A whole block and nothing else: the text goes empty, not wrong.
            (
                "<!--<string>herdr</string><string>tab</string><string>create</string>-->"
                    .to_string(),
                String::new(),
            ),
        ] {
            assert_eq!(live_text(&text, Syntax::Plist), want, "{text:?}");
        }
    }

    /// Inside `<![CDATA[ … ]]>` a comment marker is data, so a comment cannot
    /// start there and the section is carried through untouched — while a
    /// comment after it is still a comment.
    #[test]
    fn a_comment_marker_inside_cdata_is_data() {
        let launcher = "<string>herdr</string><string>agent</string><string>start</string>";
        let text = format!("<![CDATA[<!-- herdr tab create ]]>{launcher}<!-- retired -->");
        let live = live_text(&text, Syntax::Plist);
        assert_eq!(
            live,
            format!("<![CDATA[<!-- herdr tab create ]]>{launcher}")
        );
        assert!(spawns_session(&live), "{live:?}");
    }

    /// A commented-out `ExecStart` must not pull a retired sidecar's launcher
    /// back in: the unit's comments go before the stems are collected, so the
    /// file is never read at all. And a live `ExecStart` still reads it.
    #[test]
    fn a_commented_out_exec_start_does_not_read_its_sidecar() {
        let dir = unit_dir(&[(
            "collab-bot.up.sh",
            "#!/bin/bash\nexec herdr workspace create --label collab-bot\n",
        )]);
        for (unit, want) in [
            ("[Service]\n# ExecStart=/bin/bash /x/collab-bot.up.sh\nExecStart=/usr/bin/herdr-tower card\n", false),
            ("[Service]\nExecStart=/bin/bash /x/collab-bot.up.sh\n", true),
        ] {
            let text = with_sidecars(dir.path(), live_text(unit, Syntax::Unit));
            assert_eq!(
                text.contains("workspace create"),
                want,
                "the sidecar was read the wrong way: {text:?}"
            );
            assert_eq!(spawns_session(&text), want, "{text:?}");
        }
    }

    /// A sidecar's own `#` lines are its comments, and the sidecar is judged by
    /// shell rules even though the unit that named it was judged by systemd's.
    #[test]
    fn a_sidecars_comments_are_shell_comments() {
        let dir = unit_dir(&[(
            "collab-bot.up.sh",
            "#!/bin/bash\n\
             # exec herdr workspace create --label collab-bot   # the old start\n\
             exec herdr-tower card\n",
        )]);
        let unit = "[Service]\nExecStart=/bin/bash /x/collab-bot.up.sh\n";
        let text = with_sidecars(dir.path(), live_text(unit, Syntax::Unit));

        assert!(!text.contains("workspace create"), "{text:?}");
        assert!(text.contains("herdr-tower card"), "{text:?}");
        assert!(!spawns_session(&text), "{text:?}");
    }

    /// Cross validation over generated files: for the two line-commented kinds,
    /// what survives is exactly the lines a plain reading keeps, and a launcher
    /// is found when — and only when — one of those lines says a verb. The
    /// commented lines say verbs too, so a rule that kept them would be caught
    /// on the first round that puts one there.
    ///
    /// Idempotency comes with it: taking comments out of text that has none
    /// changes nothing, for all three kinds, so the pass can be repeated (a
    /// systemd unit's text is read again after its sidecar is appended).
    #[test]
    fn what_survives_a_comment_pass_is_what_a_plain_reading_keeps() {
        const LINES: [&str; 8] = [
            "ExecStart=/usr/bin/herdr agent start bot",
            "exec herdr workspace create --label collab-bot",
            "ExecStart=/usr/bin/herdr-tower card",
            "Description=a bot that says herdr tab created a place",
            "",
            "   ",
            "Environment=ARGS=--session collab-bot",
            "exec herdr session attach collab-bot",
        ];
        let mut rng = tower_testrng::Rng::new(0xc0ff_ee15_600d);

        for round in 0..500 {
            let syntax = if round % 2 == 0 {
                Syntax::Unit
            } else {
                Syntax::Shell
            };
            let marks: &[&str] = if syntax == Syntax::Unit {
                &["#", ";", "  #", "\t;"]
            } else {
                &["#", "  #", "#!", "# "]
            };

            let mut file = String::new();
            let mut live = String::new();
            for _ in 0..1 + rng.below(7) {
                let line = LINES[rng.below(LINES.len())];
                if rng.below(2) == 0 {
                    let mark = marks[rng.below(marks.len())];
                    file.push_str(mark);
                    file.push_str(line);
                } else {
                    file.push_str(line);
                    live.push_str(line);
                    live.push('\n');
                }
                file.push('\n');
            }

            // What a plain reading of the kept lines says, written out here
            // rather than asked of the code being checked: a verb as its own
            // word on a line, and `herdr` somewhere in the text.
            let wordish = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
            let says_verb = |l: &str| {
                HERDR_SPAWN_VERBS.iter().any(|verb| {
                    l.match_indices(verb).any(|(at, _)| {
                        let before = l[..at].chars().next_back();
                        let after = l[at + verb.len()..].chars().next();
                        before.is_none_or(|c| !wordish(c)) && after.is_none_or(|c| !wordish(c))
                    })
                })
            };
            let expected = live.contains("herdr") && live.lines().any(says_verb);

            let kept = live_text(&file, syntax);
            let kept_lines: Vec<&str> = kept.lines().filter(|l| !l.trim().is_empty()).collect();
            let live_lines: Vec<&str> = live.lines().filter(|l| !l.trim().is_empty()).collect();
            assert_eq!(kept_lines, live_lines, "round {round}: {file:?}");
            assert_eq!(
                spawns_session(&kept),
                expected,
                "round {round}: {file:?} -> {kept:?}"
            );
            // A second pass has nothing left to take.
            assert_eq!(live_text(&kept, syntax), kept, "round {round}: {kept:?}");
            assert_eq!(
                live_text(&kept, Syntax::Plist),
                kept,
                "round {round}: a plist rule found a comment in a shell file"
            );
        }
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The palette, matching the user's herdr theme: dark grey with an orange
//! accent (`#ffaf5f`).
//!
//! Colours are `Rgb` rather than 256-colour indices, because degrading a hex
//! colour to the nearest index would mean inspecting the terminal's declared
//! capabilities, and ratatui does not. Sending the exact value and letting the
//! terminal do the approximating is one step fewer to be wrong about, and
//! every terminal this runs in is truecolour.

use ratatui::style::{Color, Modifier, Style};

use tower_state::AgentState;

pub const HEADER: Color = Color::Rgb(0x8a, 0x8a, 0x8a);
pub const ACCENT: Color = Color::Rgb(0xff, 0xaf, 0x5f);
pub const SEL_BG: Color = Color::Rgb(0x3a, 0x3a, 0x3a);
pub const SEL_FG: Color = Color::Rgb(0xe4, 0xe4, 0xe4);
pub const HELP: Color = Color::Rgb(0x6a, 0x6a, 0x6a);
pub const HELP_KEY: Color = Color::Rgb(0xd0, 0xd0, 0xd0);
pub const MODEL: Color = Color::Rgb(0xd0, 0xd0, 0xd0);
pub const DISCORD: Color = Color::Rgb(0x7c, 0x8a, 0xff);

/// The idle loop, running.
///
/// Rose, because it is the one part of the colour wheel this palette leaves
/// empty: red 0°, accent 32°, yellow 38°, claude 38°, green 155°, session ID
/// 180°, codex 194°, preview 211°, discord 233°, gemini 275°. A loop is not a state and not a
/// vendor — it is something attached to an agent — so it must not borrow a
/// colour that already means one of those. It used to wear [`ACCENT`], which
/// is `DoneUnread`, and its preview line wore [`YELLOW`], which is `Waiting`;
/// an agent with a loop on it read as an agent in a state it was not in.
pub const LOOP: Color = Color::Rgb(0xe0, 0x7b, 0xb8);
/// The same hue, dimmed: a paused loop is the same thing, not a different one.
pub const LOOP_PAUSED: Color = Color::Rgb(0x9c, 0x5a, 0x80);

pub const RED: Color = Color::Rgb(0xf0, 0x47, 0x47);
/// The `›` between a tab's name and the pane's title — see [`tab_sep`].
///
/// Teal, which is the far side of the wheel from [`PANE_TITLE`]: the mark has
/// to be told apart from the text it separates at a glance, and violet next to
/// pink was not.
pub const SEP: Color = Color::Rgb(0x48, 0xc9, 0xb0);
/// The text of the TITLE cell — the tab's name and the pane's title alike, with
/// only [`SEP`] between them in another colour. See [`pane_title`].
pub const PANE_TITLE: Color = Color::Rgb(0xcf, 0xe3, 0x6a);
pub const YELLOW: Color = Color::Rgb(0xfa, 0xa6, 0x1a);
pub const PREVIEW: Color = Color::Rgb(0x5f, 0xaf, 0xff);
/// Dark slate teal: the captured pane reads as a panel of its own, with every
/// text colour drawn on it keeping its contrast.
pub const PREVIEW_BG: Color = Color::Rgb(0x1f, 0x30, 0x3a);
/// The session ID in the preview heading. Truecolour like the rest, not the
/// terminal's own cyan: that one is whatever the user's palette makes it, and
/// on a light scheme it all but vanishes against [`PREVIEW_BG`].
pub const CYAN: Color = Color::Rgb(0x5f, 0xd7, 0xd7);
pub const GREEN: Color = Color::Rgb(0x43, 0xb5, 0x81);
pub const WHITE: Color = Color::Rgb(0x9a, 0xa4, 0xb2);
pub const DEAD: Color = Color::Rgb(0x55, 0x55, 0x55);
/// A machine's root line — `@local` or a saved herdr machine — is a band of
/// this blue across the list, so the rows of one machine read as one block.
/// Vivid on purpose: a pale blue on the dark background did not stand out.
pub const MACHINE_BG: Color = Color::Rgb(0x1d, 0x4e, 0xd8);
/// The machine's name on that band.
pub const MACHINE_FG: Color = Color::Rgb(0xff, 0xff, 0xff);
/// The note after the name (`R to control`, `not reachable`) on the band.
pub const MACHINE_NOTE: Color = Color::Rgb(0xbf, 0xdb, 0xfe);

pub fn session_id() -> Style {
    Style::default().fg(CYAN)
}

pub fn header() -> Style {
    Style::default().fg(HEADER)
}

/// The `›` between a tab's name and the pane's title.
///
/// Coloured so the eye finds where the tab ends without reading either side:
/// the two texts meet in one cell, and a grey separator disappeared into them.
///
/// 🚨 Its own colour, not one already spoken for. A separator is not a state
/// and not a vendor, so borrowing `RED` (`Error`), `YELLOW` (`Waiting`) or
/// `ACCENT` (`DoneUnread`) would put a state's colour on every row that has a
/// tab — the same mistake the loop badge made before it was given `LOOP`.
/// Not bold either: weight is what the state marks use.
pub fn tab_sep() -> Style {
    Style::default().fg(SEP)
}

/// The TITLE cell's text: the tab's name, the pane's title, and either of them
/// alone when the other is missing.
///
/// One colour for both sides, with [`tab_sep`] the only other colour in the
/// cell. The tab used to be grey and the title the terminal's default, which
/// read as two cells rather than one — the mark between them is what says where
/// the tab ends, and it can do that while they match.
///
/// 🚨 Its own colour for the same reason the separator has one: this text is on
/// every row, so a state's or a vendor's colour here would say something about
/// every agent.
///
/// 🚨 Lime, and two colours were tried before it. Pink (`#ff87d7`) was a second
/// pink beside [`LOOP`]'s `#e07bb8` — on the same row as a loop badge, which is
/// the collision the note above warns about. Violet (`#d78fe0`) was the same
/// family once drawn: ΔE 17 from `LOOP`, 18 from `LOOP_PAUSED`, which is closer
/// to them than to anything else in the palette.
///
/// This one was measured rather than guessed. Against every colour here, in
/// CIELAB: nearest is [`ACCENT`] at ΔE 46, [`GREEN`] at 49, [`YELLOW`] at 50,
/// [`SEP`] at 58 and [`LOOP`] at 102 — the largest separation any candidate
/// reached, and the only part of the wheel this palette had left empty. Leaning
/// yellow rather than mint is what keeps it off `GREEN`, which is `Working`.
pub fn pane_title() -> Style {
    Style::default().fg(PANE_TITLE)
}

pub fn machine() -> Style {
    Style::default()
        .fg(MACHINE_FG)
        .bg(MACHINE_BG)
        .add_modifier(Modifier::BOLD)
}

pub fn machine_note() -> Style {
    Style::default().fg(MACHINE_NOTE).bg(MACHINE_BG)
}

pub fn title() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// The title that distinguishes the captured-pane panel from the tower.
///
/// Blue keeps this navigational panel heading separate from every state colour
/// while remaining bright enough to read against a dark terminal background.
pub fn preview() -> Style {
    Style::default().fg(PREVIEW).add_modifier(Modifier::BOLD)
}

pub fn preview_background() -> Style {
    Style::default().bg(PREVIEW_BG)
}

pub fn help() -> Style {
    Style::default().fg(HELP)
}

pub fn help_key() -> Style {
    Style::default().fg(HELP_KEY).add_modifier(Modifier::BOLD)
}

pub fn help_txt() -> Style {
    Style::default().fg(HEADER)
}

pub fn model() -> Style {
    Style::default().fg(MODEL)
}

/// The loop badge on an agent's row, and the loop line in the preview panel —
/// one colour, so the same thing does not look like two.
pub fn loop_badge(paused: bool) -> Style {
    if paused {
        Style::default().fg(LOOP_PAUSED)
    } else {
        Style::default().fg(LOOP).add_modifier(Modifier::BOLD)
    }
}

pub fn discord() -> Style {
    Style::default().fg(DISCORD).add_modifier(Modifier::BOLD)
}

pub fn selected() -> Style {
    Style::default()
        .bg(SEL_BG)
        .fg(SEL_FG)
        .add_modifier(Modifier::BOLD)
}

/// The colour of a state label.
pub fn state(s: AgentState) -> Style {
    let base = Style::default();
    match s {
        AgentState::Working => base.fg(GREEN),
        AgentState::Waiting => base.fg(YELLOW).add_modifier(Modifier::BOLD),
        AgentState::DoneUnread => base.fg(ACCENT).add_modifier(Modifier::BOLD),
        AgentState::Error => base.fg(RED).add_modifier(Modifier::BOLD),
        AgentState::Idle => base.fg(HEADER),
        AgentState::Dead => base.fg(DEAD),
    }
}

/// A state label in the list and the header tally.
///
/// `Waiting` is the one state that needs a person, and its amber text sat among
/// `ACCENT`, the claude bar and the ctx figures — every one of them orange. No
/// hue was left to give it (see [`LOOP`]), so it gets a shape instead: a filled
/// badge, dark text on [`YELLOW`], which no other state wears.
pub fn state_badge(s: AgentState) -> Style {
    match s {
        AgentState::Waiting => Style::default()
            .fg(Color::Rgb(0x1a, 0x1a, 0x1a))
            .bg(YELLOW)
            .add_modifier(Modifier::BOLD),
        _ => state(s),
    }
}

/// A severity word. An unknown one is drawn plainly rather than dropped — a
/// silently invisible warning is worse than an uncoloured one.
pub fn level(name: &str) -> Style {
    let base = Style::default();
    match name {
        "red" => base.fg(RED).add_modifier(Modifier::BOLD),
        "yellow" => base.fg(YELLOW),
        "wait" => base.fg(DISCORD),
        "white" => base.fg(WHITE),
        "green" => base.fg(GREEN),
        _ => base.fg(WHITE),
    }
}

/// A vendor's mark: the `▌` bar before its name, in the brand colour, and the
/// name itself in the same colour lifted 30% toward white.
///
/// 🚨 A bar, not a text colour alone. The text colours are full: fifteen of
/// them, and the four states among them (green, yellow, orange, red) take the
/// warm half of the wheel where claude's brand sits. The old text colours were
/// ΔE 15 (claude from `ACCENT`, which is `DoneUnread`), 21 (codex from `CYAN`)
/// and 26 (gemini from `DISCORD`) — a vendor that read as a state. The bar is a
/// shape no state wears, so the colour beside it says "vendor".
///
/// 🚨 Not a filled chip either. That was tried first (#157): a solid block of
/// brand colour ten cells wide, which was the loudest thing on the screen, and
/// rows of one vendor stacked into a solid column.
///
/// The name is lifted so it clears WCAG's 4.5:1 on the selection bar
/// ([`SEL_BG`]), the darkest-contrast place it is drawn; the bar, a graphic
/// rather than text, clears 3:1 there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vendor {
    pub bar: Color,
    pub name: Color,
}

/// A vendor's [`Vendor`] colours, or `None` for a key with no brand, which the
/// caller draws plainly.
pub fn provider(key: &str) -> Option<Vendor> {
    let (bar, name) = match key {
        // Anthropic's clay.
        "claude" => (Color::Rgb(0xd9, 0x77, 0x57), Color::Rgb(0xe4, 0xa0, 0x89)),
        // OpenAI's brand is black and white; its green would be `Working`.
        "codex" => (Color::Rgb(0xe8, 0xe8, 0xe8), Color::Rgb(0xef, 0xef, 0xef)),
        // Google blue. gemini and antigravity are one vendor: the tower shows
        // one row per vendor, not one per product name.
        "antigravity" | "gemini" => (Color::Rgb(0x42, 0x85, 0xf4), Color::Rgb(0x7b, 0xaa, 0xf7)),
        "opencode" => (Color::Rgb(0xa7, 0x8b, 0xfa), Color::Rgb(0xc1, 0xae, 0xfc)),
        _ => return None,
    };
    Some(Vendor { bar, name })
}

/// A vendor's bar and name colours as `(r, g, b)`, for a screen drawn with
/// escapes rather than ratatui (the usage board) — the same two colours, so
/// one vendor looks like one vendor on both.
pub fn vendor_rgb(key: &str) -> Option<[(u8, u8, u8); 2]> {
    let rgb = |c: Color| match c {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    };
    let v = provider(key)?;
    Some([rgb(v.bar)?, rgb(v.name)?])
}

/// The bar before a vendor's name.
pub const VENDOR_BAR: &str = "▌";

/// A vendor's name as spans: the bar, then `name` (already padded as the
/// caller needs it). A key with no brand gets a blank where the bar would be,
/// so its name lines up with the others.
pub fn vendor_spans(key: &str, name: String) -> [ratatui::text::Span<'static>; 2] {
    use ratatui::text::Span;
    match provider(key) {
        Some(v) => [
            Span::styled(VENDOR_BAR, Style::default().fg(v.bar)),
            Span::styled(name, Style::default().fg(v.name)),
        ],
        None => [Span::raw(" "), Span::raw(name)],
    }
}

/// Context-window pressure: yellow from 40%, red from 80%.
///
/// 40% is not a round number picked for looks — it is where this user starts
/// closing sessions out, so the colour has to change while there is still room
/// to act.
pub fn ctx(used: f64) -> Style {
    if used >= 80.0 {
        level("red")
    } else if used >= 40.0 {
        level("yellow")
    } else {
        level("green")
    }
    .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loop is not a state and not a vendor, so its colour must not be one
    /// of theirs. This is the whole reason it has its own: it wore `ACCENT`
    /// (`DoneUnread`) on the agent row and `YELLOW` (`Waiting`) in the
    /// preview, and an agent with a loop attached read as an agent in a state
    /// it was not in.
    #[test]
    fn the_loop_colour_is_not_borrowed_from_a_state_or_a_vendor() {
        let taken = [
            ACCENT, YELLOW, RED, GREEN, HEADER, DEAD, PREVIEW, DISCORD, WHITE, MODEL, CYAN,
        ];
        for colour in taken {
            assert_ne!(LOOP, colour);
            assert_ne!(LOOP_PAUSED, colour);
            // The tab separator is not a state or a vendor either, and it is on
            // every row that has a tab — a borrowed colour there would be the
            // loop's old mistake on far more rows.
            assert_ne!(SEP, colour);
            assert_ne!(PANE_TITLE, colour);
        }
        for vendor in ["claude", "codex", "gemini", "antigravity", "opencode"] {
            let v = provider(vendor).expect("a known vendor has a colour");
            for mine in [LOOP, LOOP_PAUSED, SEP, PANE_TITLE] {
                assert_ne!(v.bar, mine);
                assert_ne!(v.name, mine);
            }
        }
        // The separator and the title are not the loop, and not each other —
        // the whole point of colouring them is telling the three apart on one
        // row.
        for mine in [SEP, PANE_TITLE] {
            assert_ne!(mine, LOOP);
            assert_ne!(mine, LOOP_PAUSED);
        }
        assert_ne!(SEP, PANE_TITLE);
    }

    /// `Waiting` is the one state drawn as a filled badge; every other state
    /// keeps its plain text colour, so the badge stays the only one of its kind.
    #[test]
    fn only_waiting_is_a_filled_badge() {
        let wait = state_badge(AgentState::Waiting);
        assert_eq!(wait.bg, Some(YELLOW));
        assert!(wait.add_modifier.contains(Modifier::BOLD));
        for s in [
            AgentState::Working,
            AgentState::DoneUnread,
            AgentState::Error,
            AgentState::Idle,
            AgentState::Dead,
        ] {
            assert_eq!(state_badge(s), state(s));
            assert_eq!(state_badge(s).bg, None);
        }
    }

    /// Every kind the tower supervises gets a mark, and no two share one — a
    /// kind without a colour would be the one row the eye skips.
    #[test]
    fn every_supervised_kind_has_its_own_mark() {
        let mut seen: Vec<Color> = Vec::new();
        for kind in tower_state::SUPERVISED_KINDS {
            let v = provider(kind).unwrap_or_else(|| panic!("{kind} has no mark"));
            assert!(!seen.contains(&v.bar), "{kind} shares a bar colour");
            seen.push(v.bar);
        }
        assert_eq!(provider("antigravity"), provider("gemini"));
        assert_eq!(provider("bash"), None);
    }

    fn lum(c: Color) -> f64 {
        let Color::Rgb(r, g, b) = c else {
            panic!("vendor colours are truecolour");
        };
        let f = |v: u8| {
            let v = f64::from(v) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
    }

    fn contrast(a: Color, b: Color) -> f64 {
        let (hi, lo) = (lum(a).max(lum(b)), lum(a).min(lum(b)));
        (hi + 0.05) / (lo + 0.05)
    }

    /// The selected row is the hardest place to read a vendor: the name must
    /// clear WCAG's 4.5:1 for text on [`SEL_BG`], and the bar 3:1 for a
    /// graphic. The name is also the lighter of the two, so it never reads as
    /// the louder one.
    #[test]
    fn a_vendor_is_readable_on_the_selection_bar() {
        for kind in tower_state::SUPERVISED_KINDS {
            let v = provider(kind).unwrap();
            let name = contrast(v.name, SEL_BG);
            let bar = contrast(v.bar, SEL_BG);
            assert!(name >= 4.5, "{kind}: name contrast {name:.2} < 4.5");
            assert!(bar >= 3.0, "{kind}: bar contrast {bar:.2} < 3");
            assert!(lum(v.name) >= lum(v.bar), "{kind}: the name is lifted");
        }
    }

    /// The escape-drawn screens get the very colours the tower draws with.
    #[test]
    fn the_rgb_pair_is_the_vendors_own_colours() {
        for kind in tower_state::SUPERVISED_KINDS {
            let v = provider(kind).unwrap();
            let [(br, bg, bb), (nr, ng, nb)] = vendor_rgb(kind).unwrap();
            assert_eq!(v.bar, Color::Rgb(br, bg, bb), "{kind}");
            assert_eq!(v.name, Color::Rgb(nr, ng, nb), "{kind}");
        }
        assert_eq!(vendor_rgb("bash"), None);
    }

    /// An unbranded kind gets a blank in the bar's cell: its name lines up
    /// with the others, and nothing coloured claims it for a vendor.
    #[test]
    fn an_unbranded_kind_keeps_the_bars_cell_blank() {
        let [bar, name] = vendor_spans("bash", "bash".into());
        assert_eq!(bar.content, " ");
        assert_eq!(bar.style, Style::default());
        assert_eq!(name.content, "bash");
        let [bar, _] = vendor_spans("codex", "codex".into());
        assert_eq!(bar.content, VENDOR_BAR);
        assert_eq!(tower_text::width(VENDOR_BAR), 1);
    }

    /// Paused is the same loop, dimmed — one hue, two weights, so the badge
    /// and the preview line stay recognizably the same thing.
    #[test]
    fn a_paused_loop_is_the_same_colour_dimmed() {
        let running = loop_badge(false);
        let paused = loop_badge(true);
        assert_eq!(running.fg, Some(LOOP));
        assert_eq!(paused.fg, Some(LOOP_PAUSED));
        assert!(running.add_modifier.contains(Modifier::BOLD));
        assert!(!paused.add_modifier.contains(Modifier::BOLD));

        let (Color::Rgb(lr, lg, lb), Color::Rgb(pr, pg, pb)) = (LOOP, LOOP_PAUSED) else {
            panic!("both loop colours are truecolour");
        };
        assert!(
            u32::from(pr) + u32::from(pg) + u32::from(pb)
                < u32::from(lr) + u32::from(lg) + u32::from(lb),
            "the paused colour is the darker of the two"
        );
    }
}

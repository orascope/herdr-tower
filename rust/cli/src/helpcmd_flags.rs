// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The command-line flag parser, reduced to what three commands here need.
//!
//! What matters is the interface, not the implementation. `restore`,
//! `wake-all`/`close-all`/`broadcast` and every `wt` subcommand parse their
//! flags with it, so its accidents are the CLI's documented behaviour: a single
//! leading dash and a double one mean the same thing, a boolean flag never
//! consumes the next argument, parsing stops at the first non-flag word, and
//! `-h` on a set that defines no `h` prints the defaults and fails. A parser
//! that got any of those "right" would break scripts.
//!
//! `expected/restore-help.txt` is the reason the usage block is reproduced down
//! to its whitespace: four spaces then a tab, so the text lines up under both
//! 4- and 8-column tab stops.
//!
//! Durations are here for the same reason. `--timeout 10m` is parsed by
//! [`parse_duration`], and the value comes back out in messages
//! (`watching for completion (timeout 10m0s)`) through [`format_duration`], which
//! always writes the seconds component — `10m` in, `10m0s` out.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::Duration;

/// What a flag holds. Only the three shapes the command line actually uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Never takes the following argument — `-yes true` is a flag and a
    /// positional, not one flag with a value. Only `-yes=false` sets it false.
    Bool,
    Str,
    Duration,
}

impl Kind {
    /// The placeholder [`FlagSet::usage`] writes after the flag name.
    ///
    /// `None` for booleans, so the line reads `-yes` rather than `-yes bool`.
    fn type_name(self) -> Option<&'static str> {
        match self {
            Kind::Bool => None,
            Kind::Str => Some("string"),
            Kind::Duration => Some("duration"),
        }
    }

    /// The default that [`FlagSet::usage`] treats as "no default worth printing".
    fn zero(self) -> &'static str {
        match self {
            Kind::Bool => "false",
            Kind::Str => "",
            Kind::Duration => "0s",
        }
    }
}

struct Flag {
    name: String,
    kind: Kind,
    usage: String,
    /// Stored as a string, whatever the type.
    default: String,
}

/// Refusals, each with a fixed spelling.
///
/// The text matters: it reaches the user through `herdr-tower: <message>`, and
/// `expected/restore-help.txt` ends on exactly the `flag: help requested` line.
#[derive(Debug, PartialEq, Eq)]
pub enum FlagError {
    /// `-h`/`--help` on a set that defines no such flag.
    Help,
    /// Anything else — an undefined flag, a missing value, an unparseable one.
    Bad(String),
}

impl std::fmt::Display for FlagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlagError::Help => write!(f, "flag: help requested"),
            FlagError::Bad(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for FlagError {}

/// One command's flags, and the positional words left over.
#[derive(Debug, PartialEq, Eq)]
pub struct Parsed {
    values: HashMap<String, String>,
    args: Vec<String>,
}

impl Parsed {
    /// Panics on an undeclared name, which can only be a typo in this crate —
    /// the value map is seeded from the declarations, so a name that was
    /// declared is always present.
    fn raw(&self, name: &str) -> &str {
        self.values
            .get(name)
            .unwrap_or_else(|| panic!("flag {name:?} was never declared"))
    }

    pub fn bool(&self, name: &str) -> bool {
        self.raw(name) == "true"
    }

    pub fn str(&self, name: &str) -> &str {
        self.raw(name)
    }

    /// The stored text is only ever written by [`FlagSet::duration`] or by a
    /// parse that already succeeded, so this cannot fail.
    pub fn duration(&self, name: &str) -> Duration {
        parse_duration(self.raw(name)).unwrap_or_default()
    }

    /// The words after the flags.
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

/// A command's flag declarations.
pub struct FlagSet {
    name: String,
    flags: Vec<Flag>,
}

impl FlagSet {
    /// `name` is what `Usage of <name>:` prints, so it is the subcommand as
    /// the user typed it (`"restore"`, `"wt new"`).
    pub fn new(name: impl Into<String>) -> Self {
        FlagSet {
            name: name.into(),
            flags: Vec::new(),
        }
    }

    pub fn bool(mut self, name: &str, default: bool, usage: &str) -> Self {
        self.push(name, Kind::Bool, usage, default.to_string());
        self
    }

    pub fn string(mut self, name: &str, default: &str, usage: &str) -> Self {
        self.push(name, Kind::Str, usage, default.to_string());
        self
    }

    pub fn duration(mut self, name: &str, default: Duration, usage: &str) -> Self {
        self.push(name, Kind::Duration, usage, format_duration(default));
        self
    }

    fn push(&mut self, name: &str, kind: Kind, usage: &str, default: String) {
        self.flags.push(Flag {
            name: name.to_string(),
            kind,
            usage: usage.to_string(),
            default,
        });
    }

    fn find(&self, name: &str) -> Option<&Flag> {
        self.flags.iter().find(|f| f.name == name)
    }

    /// The defaults block with the `Usage of <name>:` header above it.
    ///
    /// Flags come out in plain lexicographic order — not declaration order, so
    /// `-dry-run` precedes `-resume` in `restore`'s help even though the code
    /// declares `-resume` first.
    pub fn usage(&self) -> String {
        let mut sorted: Vec<&Flag> = self.flags.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));

        let mut out = format!("Usage of {}:\n", self.name);
        for f in sorted {
            let mut line = format!("  -{}", f.name);
            if let Some(t) = f.kind.type_name() {
                line.push(' ');
                line.push_str(t);
            }
            // A one-letter boolean fits beside its description; anything else
            // gets its own line. Four spaces then a tab, so the text lands in
            // the same column under either tab width.
            if line.len() <= 4 {
                line.push('\t');
            } else {
                line.push_str("\n    \t");
            }
            line.push_str(&f.usage.replace('\n', "\n    \t"));
            if f.default != f.kind.zero() {
                match f.kind {
                    Kind::Str => {
                        let _ = write!(line, " (default {:?})", f.default);
                    }
                    _ => {
                        let _ = write!(line, " (default {})", f.default);
                    }
                }
            }
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// What the parser writes to its own output *before* handing the error
    /// back, so a caller can reproduce both halves of what the binary prints.
    ///
    /// That is the message and then the usage, and `main` prints the same
    /// message again prefixed with `herdr-tower:`. The duplication is real and
    /// preserved.
    pub fn diagnostic(&self, e: &FlagError) -> String {
        match e {
            FlagError::Help => self.usage(),
            FlagError::Bad(m) => format!("{m}\n{}", self.usage()),
        }
    }

    /// Parses `args`, handing back the first refusal instead of exiting.
    pub fn parse<S: AsRef<str>>(&self, args: &[S]) -> Result<Parsed, FlagError> {
        let mut values: HashMap<String, String> = self
            .flags
            .iter()
            .map(|f| (f.name.clone(), f.default.clone()))
            .collect();
        let args: Vec<&str> = args.iter().map(|a| a.as_ref()).collect();

        let mut i = 0;
        while i < args.len() {
            let s = args[i];
            // Not a flag: everything from here is positional, and stays
            // positional even if it looks like a flag later on.
            if s.len() < 2 || !s.starts_with('-') {
                break;
            }
            i += 1;
            let mut name = &s[1..];
            if name.starts_with('-') {
                name = &name[1..];
                // A bare `--` ends the flags and is itself consumed.
                if name.is_empty() {
                    break;
                }
            }
            if name.starts_with('-') || name.starts_with('=') {
                return Err(FlagError::Bad(format!("bad flag syntax: {s}")));
            }

            let mut value: Option<String> = None;
            if let Some(eq) = name.find('=') {
                value = Some(name[eq + 1..].to_string());
                name = &name[..eq];
            }

            let Some(f) = self.find(name) else {
                if name == "help" || name == "h" {
                    return Err(FlagError::Help);
                }
                return Err(FlagError::Bad(format!(
                    "flag provided but not defined: -{name}"
                )));
            };

            let text = match f.kind {
                Kind::Bool => match value {
                    // `-yes=false` is the only way to turn one off, because a
                    // boolean that swallowed the next word would eat the
                    // positional argument after it.
                    Some(v) => parse_bool(&v).map(|b| b.to_string()).ok_or_else(|| {
                        FlagError::Bad(format!(
                            "invalid boolean value {v:?} for -{name}: parse error"
                        ))
                    })?,
                    None => "true".to_string(),
                },
                _ => {
                    let v = match value {
                        Some(v) => v,
                        None => {
                            let next = args.get(i).ok_or_else(|| {
                                FlagError::Bad(format!("flag needs an argument: -{name}"))
                            })?;
                            i += 1;
                            next.to_string()
                        }
                    };
                    if f.kind == Kind::Duration && parse_duration(&v).is_none() {
                        return Err(FlagError::Bad(format!(
                            "invalid value {v:?} for flag -{name}: parse error"
                        )));
                    }
                    v
                }
            };
            values.insert(name.to_string(), text);
        }

        Ok(Parsed {
            values,
            args: args[i..].iter().map(|s| s.to_string()).collect(),
        })
    }
}

/// A boolean flag value, which is laxer than it looks.
fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// A duration such as `1h30m` or `1.5s`, restricted to non-negative values.
///
/// A `std::time::Duration` cannot be negative, and no flag here means anything
/// with a negative timeout, so a leading `-` is refused rather than silently
/// made positive.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let body = s.strip_prefix('+').unwrap_or(s);
    if body.starts_with('-') {
        return None;
    }
    // A bare "0" needs no unit, and it is the only number that does.
    if body == "0" {
        return Some(Duration::ZERO);
    }
    if body.is_empty() {
        return None;
    }

    let mut nanos: f64 = 0.0;
    let mut rest = body;
    let mut saw_one = false;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        if digits == 0 {
            return None;
        }
        let n: f64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];

        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let scale = match &rest[..unit_len] {
            "ns" => 1.0,
            "us" | "µs" | "μs" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            // Includes the empty unit: a missing unit is a refusal, not a
            // default of seconds.
            _ => return None,
        };
        rest = &rest[unit_len..];
        nanos += n * scale;
        saw_one = true;
    }
    if !saw_one {
        return None;
    }
    // A duration too large to represent must be refused: the float
    // accumulation above becomes infinity for a long enough digit string,
    // `as u64` then *saturates* rather than overflowing, and the caller gets
    // `u64::MAX` nanoseconds. Callers add this to an `Instant`, an
    // `OffsetDateTime`, or the socket deadline's slack — all of which panic on
    // overflow, so a mistyped `--timeout` would kill `close-all --watch` or
    // `wt wait` instead of being rejected.
    (nanos.is_finite() && nanos >= 0.0 && nanos < u64::MAX as f64)
        .then(|| Duration::from_nanos(nanos as u64))
}

/// A duration as the user sees it: `10m0s`, `1.5ms`, `1h0m0s`.
///
/// Exact rather than approximate because the value is *shown*:
/// `--timeout 10m` echoes back as `10m0s` in the watch banner and as
/// `(default 10m0s)` in the usage block. The seconds component is always
/// written once the duration reaches a second, and minutes appear whenever
/// hours do, however zero they are — `1h0m0s`.
pub fn format_duration(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos == 0 {
        return "0s".to_string();
    }
    if nanos < 1_000_000_000 {
        let (unit, div) = if nanos < 1_000 {
            ("ns", 1u128)
        } else if nanos < 1_000_000 {
            ("µs", 1_000)
        } else {
            ("ms", 1_000_000)
        };
        return format!("{}{unit}", trim_frac(nanos, div));
    }

    let mut out = format!("{}s", trim_frac(nanos % 60_000_000_000, 1_000_000_000));
    let minutes = nanos / 60_000_000_000;
    if minutes > 0 {
        out = format!("{}m{out}", minutes % 60);
        let hours = minutes / 60;
        if hours > 0 {
            out = format!("{hours}h{out}");
        }
    }
    out
}

/// `whole.fraction` with trailing zeros dropped, and the point dropped with
/// them.
fn trim_frac(value: u128, div: u128) -> String {
    let whole = value / div;
    let frac = value % div;
    if frac == 0 {
        return whole.to_string();
    }
    let width = div.to_string().len() - 1;
    let digits = format!("{frac:0width$}");
    format!("{whole}.{}", digits.trim_end_matches('0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn restore_set() -> FlagSet {
        FlagSet::new("restore")
            .bool("resume", false, "revive the conversation too")
            .bool("dry-run", false, "print the plan and change nothing")
            .bool("yes", false, "do all of them")
    }

    /// One dash and two are the same flag, because the parser has never
    /// distinguished them and every script written against this CLI picked one
    /// arbitrarily.
    #[test]
    fn one_dash_and_two_name_the_same_flag() {
        let fs = restore_set();
        assert!(fs.parse(&["-resume"]).unwrap().bool("resume"));
        assert!(fs.parse(&["--resume"]).unwrap().bool("resume"));
    }

    /// A boolean must not swallow the word after it: `restore --yes claude-1`
    /// asks to restore `claude-1`, not to parse `claude-1` as a truth value.
    #[test]
    fn a_boolean_never_consumes_the_next_argument() {
        let p = restore_set().parse(&["--yes", "claude-1"]).unwrap();
        assert!(p.bool("yes"));
        assert_eq!(p.args(), ["claude-1"]);
    }

    /// `-yes=false` is the only way to turn one off, and it is honoured.
    #[test]
    fn an_explicit_false_turns_a_boolean_off() {
        let fs = FlagSet::new("wake-all").bool("watch", true, "watch");
        assert!(!fs.parse(&["--watch=false"]).unwrap().bool("watch"));
    }

    /// Parsing stops at the first word that is not a flag, so anything after a
    /// positional stays positional. That is the documented behaviour, and the
    /// reason `wt` re-orders `task --flag` by hand before parsing.
    #[test]
    fn a_flag_after_a_positional_is_left_as_a_positional() {
        let p = restore_set().parse(&["claude-1", "--yes"]).unwrap();
        assert!(!p.bool("yes"));
        assert_eq!(p.args(), ["claude-1", "--yes"]);
    }

    #[test]
    fn a_bare_double_dash_ends_the_flags_and_is_consumed() {
        let p = restore_set().parse(&["--", "--yes"]).unwrap();
        assert!(!p.bool("yes"));
        assert_eq!(p.args(), ["--yes"]);
    }

    #[test]
    fn an_undefined_flag_names_itself_in_the_refusal() {
        let e = restore_set().parse(&["--nope"]).unwrap_err();
        assert_eq!(e.to_string(), "flag provided but not defined: -nope");
    }

    /// `-h` is not declared anywhere, so it falls through to the help case
    /// rather than to "not defined".
    #[test]
    fn h_and_help_ask_for_the_usage_block() {
        for arg in ["-h", "--help", "-help"] {
            assert_eq!(restore_set().parse(&[arg]), Err(FlagError::Help), "{arg}");
        }
    }

    #[test]
    fn a_value_flag_takes_the_next_word_or_says_it_is_missing() {
        let fs = FlagSet::new("wt new").string("agent", "claude", "kind");
        assert_eq!(
            fs.parse(&["--agent", "codex"]).unwrap().str("agent"),
            "codex"
        );
        assert_eq!(fs.parse(&["--agent=codex"]).unwrap().str("agent"), "codex");
        assert_eq!(
            fs.parse(&["--agent"]).unwrap_err().to_string(),
            "flag needs an argument: -agent"
        );
    }

    /// The defaults block is sorted by name, not by declaration order, and a
    /// non-zero default is printed while a zero one is not.
    #[test]
    fn the_usage_block_sorts_by_name_and_hides_zero_defaults() {
        let fs = FlagSet::new("wake-all")
            .bool("yes", false, "send without asking")
            .string("except", "", "sessions to leave out")
            .duration("timeout", Duration::from_secs(600), "how long to watch")
            .bool("watch", true, "watch for completion");
        assert_eq!(
            fs.usage(),
            "Usage of wake-all:\n\
             \x20 -except string\n    \tsessions to leave out\n\
             \x20 -timeout duration\n    \thow long to watch (default 10m0s)\n\
             \x20 -watch\n    \twatch for completion (default true)\n\
             \x20 -yes\n    \tsend without asking\n"
        );
    }

    /// `10m` in, `10m0s` out: the value is shown back to the user, so the
    /// formatting is part of the interface.
    #[test]
    fn durations_round_trip_as_printed() {
        for (text, want) in [
            ("10m", "10m0s"),
            ("30m", "30m0s"),
            ("90s", "1m30s"),
            ("1h30m", "1h30m0s"),
            ("1h", "1h0m0s"),
            ("500ms", "500ms"),
            ("1500us", "1.5ms"),
            ("0", "0s"),
        ] {
            let d = parse_duration(text).unwrap_or_else(|| panic!("{text} did not parse"));
            assert_eq!(format_duration(d), want, "{text}");
        }
    }

    #[test]
    fn a_duration_without_a_unit_is_refused_rather_than_read_as_seconds() {
        assert_eq!(parse_duration("10"), None);
        assert_eq!(parse_duration("10x"), None);
        assert_eq!(parse_duration("-5m"), None);
    }

    /// The laxness is the interface: `-watch=1`, `-watch=T` and `-watch=True`
    /// all turn a flag on, and a script that picked any of those spellings has
    /// been working since the CLI shipped.
    #[test]
    fn a_boolean_takes_every_spelling_it_has_always_taken() {
        let on = FlagSet::new("wake-all").bool("watch", false, "watch");
        for v in ["1", "t", "T", "TRUE", "true", "True"] {
            assert!(
                on.parse(&[format!("--watch={v}")]).unwrap().bool("watch"),
                "{v:?}"
            );
        }
        let off = FlagSet::new("wake-all").bool("watch", true, "watch");
        for v in ["0", "f", "F", "FALSE", "false", "False"] {
            assert!(
                !off.parse(&[format!("--watch={v}")]).unwrap().bool("watch"),
                "{v:?}"
            );
        }
    }

    /// Everything else is refused with the value quoted back, including the
    /// near misses somebody would expect to work. An empty `-watch=` is one of
    /// them: it is a typo, not a request to turn the flag off, and reading it
    /// as `false` would quietly stop a watch that was asked for.
    #[test]
    fn a_boolean_refuses_every_other_value_and_quotes_it_back() {
        let fs = FlagSet::new("wake-all").bool("watch", false, "watch");
        for bad in ["", " ", "yes", "no", "on", "TrUe", "2", "-1", "true ", "예"] {
            assert_eq!(
                fs.parse(&[format!("--watch={bad}")])
                    .unwrap_err()
                    .to_string(),
                format!("invalid boolean value {bad:?} for -watch: parse error"),
                "{bad:?}"
            );
        }
    }

    /// A duration flag checks its value as it parses, so `--timeout 10` is
    /// refused at the command line rather than falling back to the default and
    /// watching for a length of time nobody asked for.
    ///
    /// The last case is the default's own round trip: it is stored as the text
    /// [`format_duration`] produced, and reading it back has to return the
    /// duration it was declared with.
    #[test]
    fn a_duration_flag_refuses_a_value_it_cannot_parse() {
        let fs = FlagSet::new("wake-all").duration("timeout", Duration::from_secs(600), "how long");
        for bad in ["10", "abc", "", "-5m", "10 m", "1h30", "10M"] {
            assert_eq!(
                fs.parse(&[format!("--timeout={bad}")])
                    .unwrap_err()
                    .to_string(),
                format!("invalid value {bad:?} for flag -timeout: parse error"),
                "{bad:?}"
            );
        }
        assert_eq!(
            fs.parse(&["--timeout", "90s"]).unwrap().duration("timeout"),
            Duration::from_secs(90)
        );
        assert_eq!(
            fs.parse::<&str>(&[]).unwrap().duration("timeout"),
            Duration::from_secs(600),
            "the default survives being stored as its own printed form"
        );
    }

    /// The shapes that are not quite a duration, each of which reaches the
    /// parser from a real command line: a bare sign, digits with no unit, and
    /// the padding a shell or a copied banner leaves behind.
    #[test]
    fn a_duration_that_is_not_quite_a_duration_is_refused() {
        for bad in [
            "", " ", "+", "-", "m", "h30m", "10 m", " 10m", "10m ", "00", "1.5", "10S",
        ] {
            assert_eq!(parse_duration(bad), None, "{bad:?}");
        }
    }

    /// The shapes that are accepted and would be easy to lose: a leading plus,
    /// both micro signs a terminal may produce, units in sequence, and the bare
    /// zero that is the one number allowed to carry no unit.
    #[test]
    fn a_duration_accepts_a_plus_both_micro_signs_and_a_bare_zero() {
        assert_eq!(parse_duration("+10m"), Some(Duration::from_secs(600)));
        assert_eq!(parse_duration("0"), Some(Duration::ZERO));
        assert_eq!(parse_duration("+0"), Some(Duration::ZERO));
        assert_eq!(parse_duration("0s"), Some(Duration::ZERO));
        assert_eq!(parse_duration("0h0m0s"), Some(Duration::ZERO));
        for micro in ["1500us", "1500µs", "1500μs"] {
            assert_eq!(
                parse_duration(micro),
                Some(Duration::from_micros(1500)),
                "{micro}"
            );
        }
        assert_eq!(parse_duration("1h30m10s"), Some(Duration::from_secs(5410)));
        assert_eq!(
            parse_duration("1m1m"),
            Some(Duration::from_secs(120)),
            "a repeated unit adds up rather than replacing"
        );
    }
}

/// Generated durations, checked against the arithmetic they are meant to do.
///
/// Hand-rolled rather than a property framework: the generator is seeded, so a
/// failure replays exactly and nothing is added to the build.
#[cfg(test)]
mod generated_tests {
    use super::*;
    use tower_testrng::Rng;

    /// The printed form is the only form the user ever hands back — `10m0s` out
    /// of the watch banner goes straight into the next `--timeout` — so every
    /// duration this prints has to parse back to the same duration.
    ///
    /// Drawn from the durations [`format_duration`] prints **without a
    /// fraction**, because that is the range where the round trip is exact and
    /// it is the range every flag here lives in: a whole number of seconds
    /// (`1h30m10s`), a whole number of milliseconds or microseconds (`500ms`,
    /// `750µs`), and anything under a microsecond (`999ns`). The parse then
    /// adds whole multiples of a unit's nanosecond scale in `f64`, and those are
    /// exact while the sum stays under 2^53 nanoseconds — eleven days here,
    /// against the hundred-odd days where the limit actually bites.
    ///
    /// A printed *fraction* is a different matter and does not round trip.
    /// [`a_printed_fraction_parses_back_a_nanosecond_low`] pins that as it is,
    /// rather than this test hiding it in a tolerance or in a comment.
    #[test]
    fn a_printed_duration_without_a_fraction_parses_back_to_itself() {
        let mut rng = Rng::new(0x5EED);
        for _ in 0..20_000 {
            for d in [
                Duration::from_secs(rng.below(1_000_000) as u64),
                Duration::from_millis(rng.below(1_000) as u64),
                Duration::from_micros(rng.below(1_000) as u64),
                Duration::from_nanos(rng.below(1_000) as u64),
            ] {
                let text = format_duration(d);
                assert!(
                    !text.contains('.'),
                    "{text:?} came from {d:?} and carries a fraction"
                );
                assert_eq!(parse_duration(&text), Some(d), "{text:?} came from {d:?}");
            }
        }
    }

    /// And the limit of that, written down as a test rather than as a caveat: a
    /// printed fraction can come back one nanosecond low.
    ///
    /// The parse multiplies the fraction by its unit's scale in `f64` and then
    /// *truncates*, so `2.098444162` × 1e9 lands a hair under 2098444162 and
    /// `as u64` drops the rest. It is not every fraction — a fraction a binary
    /// float names exactly, `1.5`, is fine — and it is never more than one
    /// nanosecond, and no flag here is ever handed nanosecond precision. But it
    /// is real, and a test that generated over this range would fail on some
    /// seeds and not others, which is worse than saying so.
    #[test]
    fn a_printed_fraction_parses_back_a_nanosecond_low() {
        // One nanosecond low: the printed fraction is not a binary fraction.
        for (nanos, printed) in [
            (1_001u64, "1.001µs"),
            (1_000_001, "1.000001ms"),
            (2_000_000_003, "2.000000003s"),
            (2_098_444_162, "2.098444162s"),
        ] {
            let d = Duration::from_nanos(nanos);
            assert_eq!(format_duration(d), printed, "{nanos}");
            assert_eq!(
                parse_duration(printed),
                Some(Duration::from_nanos(nanos - 1)),
                "{printed} is one nanosecond low, not exact"
            );
        }
        // Exact all the same, wherever the fraction is one a binary float names
        // or the division comes out whole.
        for (nanos, printed) in [
            (999u64, "999ns"),
            (1_500, "1.5µs"),
            (1_500_000, "1.5ms"),
            (999_999_999, "999.999999ms"),
            (2_500_000_000, "2.5s"),
        ] {
            let d = Duration::from_nanos(nanos);
            assert_eq!(format_duration(d), printed, "{nanos}");
            assert_eq!(parse_duration(printed), Some(d), "{printed}");
        }
    }

    /// The printed form against the hours-minutes-seconds anybody would work
    /// out by hand. [`format_duration`] reaches its answer another way —
    /// remainders against nanosecond constants, a string built from the inside
    /// out — and the two have to agree on every value.
    #[test]
    fn whole_seconds_print_as_a_naive_h_m_s_computation() {
        let mut rng = Rng::new(7);
        for _ in 0..20_000 {
            let secs = rng.below(10_000_000) as u64;
            let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
            let want = if h > 0 {
                format!("{h}h{m}m{s}s")
            } else if m > 0 {
                format!("{m}m{s}s")
            } else {
                format!("{s}s")
            };
            assert_eq!(format_duration(Duration::from_secs(secs)), want, "{secs}s");
        }
    }

    /// And the parse in the same terms: `1h30m10s` is 5410 seconds whichever
    /// way it is counted, so the unit scaling is checked against plain
    /// multiplication rather than against itself.
    #[test]
    fn an_h_m_s_duration_parses_to_the_seconds_it_names() {
        let mut rng = Rng::new(11);
        for _ in 0..20_000 {
            let (h, m, s) = (
                rng.below(10_000) as u64,
                rng.below(60) as u64,
                rng.below(60) as u64,
            );
            let text = format!("{h}h{m}m{s}s");
            assert_eq!(
                parse_duration(&text).map(|d| d.as_secs()),
                Some(h * 3600 + m * 60 + s),
                "{text}"
            );
        }
    }
}

#[cfg(test)]
mod overflow_tests {
    use super::*;

    /// A duration this large is a typo, and it is refused.
    ///
    /// The float accumulation becomes infinity, `as u64` then *saturates*
    /// rather than overflowing, and the caller silently receives `u64::MAX`
    /// nanoseconds. Callers add that to an `Instant`, an `OffsetDateTime` or the
    /// socket deadline's slack, all of which panic on overflow — so a mistyped
    /// `--timeout` would kill `close-all --watch` or `wt wait` rather than being
    /// rejected at the flag.
    #[test]
    fn a_duration_too_large_to_represent_is_refused() {
        for bad in [
            "99999999999999999999999999999999h",
            "1e308h",
            "184467440737095516160s",
            "9223372036854775807h",
        ] {
            assert_eq!(
                parse_duration(bad),
                None,
                "{bad:?} should be refused, not saturated"
            );
        }
        // The ordinary values still parse, including the largest sane timeout.
        assert_eq!(parse_duration("10m"), Some(Duration::from_secs(600)));
        assert_eq!(parse_duration("1h30m"), Some(Duration::from_secs(5400)));
        assert!(parse_duration("100000h").is_some(), "11 years is fine");
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! A single error type for every command invocation.
//!
//! The variants exist so the exit path can distinguish a local store failure
//! from a herdr socket error from a user-facing message, and so every branch
//! that adds `.into()` on a string is visible in one place.
//!
//! The `From` implementations deliberately accept everything `Box<dyn Error>`
//! did, so existing call sites compile without changes during the transition.

/// Everything that can go wrong in a command invocation.
#[derive(Debug)]
pub enum CliError {
    /// The record store could not be opened or written to.
    Store(std::io::Error),
    /// The herdr socket is unreachable or answered with an error.
    Herdr(herdrx::Error),
    /// A user-facing message that needs no backtrace.
    Message(String),
    /// Anything else — a bridge for code that still returns `Box<dyn Error>`.
    Other(Box<dyn std::error::Error + 'static>),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Store(e) => write!(f, "{e}"),
            CliError::Herdr(e) => write!(f, "{e}"),
            CliError::Message(m) => write!(f, "{m}"),
            CliError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CliError::Store(e) => Some(e),
            CliError::Herdr(e) => Some(e),
            CliError::Message(_) => None,
            CliError::Other(e) => Some(e.as_ref()),
        }
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::Store(e)
    }
}

impl From<herdrx::Error> for CliError {
    fn from(e: herdrx::Error) -> Self {
        CliError::Herdr(e)
    }
}

impl From<String> for CliError {
    fn from(m: String) -> Self {
        CliError::Message(m)
    }
}

impl From<&str> for CliError {
    fn from(m: &str) -> Self {
        CliError::Message(m.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(e: serde_json::Error) -> Self {
        CliError::Other(Box::new(e))
    }
}

impl From<tower_mux::Error> for CliError {
    fn from(e: tower_mux::Error) -> Self {
        CliError::Other(Box::new(e))
    }
}

impl From<tower_wt::Error> for CliError {
    fn from(e: tower_wt::Error) -> Self {
        CliError::Other(Box::new(e))
    }
}

impl From<tower_cli::helpcmd::FlagError> for CliError {
    fn from(e: tower_cli::helpcmd::FlagError) -> Self {
        CliError::Other(Box::new(e))
    }
}

/// Blanket bridge: anything that was `Box<dyn Error>` still converts.
impl From<Box<dyn std::error::Error + 'static>> for CliError {
    fn from(e: Box<dyn std::error::Error + 'static>) -> Self {
        CliError::Other(e)
    }
}

/// The result type for command implementations.
pub type Result<T> = std::result::Result<T, CliError>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn display_and_source_chains_are_preserved() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let cli_store = CliError::from(io_err);
        assert_eq!(cli_store.to_string(), "file missing");
        assert!(cli_store.source().is_some());

        let msg_err = CliError::from("custom failure");
        assert_eq!(msg_err.to_string(), "custom failure");
        assert!(msg_err.source().is_none());

        let herdr_err = CliError::from(herdrx::Error::NoSocket);
        assert!(herdr_err.to_string().contains("socket"));
        assert!(herdr_err.source().is_some());

        let json_err: CliError = serde_json::from_str::<serde_json::Value>("invalid")
            .unwrap_err()
            .into();
        assert!(matches!(json_err, CliError::Other(_)));
        assert!(json_err.source().is_some());
    }

    /// A small deterministic generator (an LCG), so the property test walks
    /// the same inputs on every run and a failure can be replayed.
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }

    /// `main` prints `herdr-tower: {e}` and nothing else, so every conversion
    /// has to display exactly what the error it wraps displays — checked
    /// against that error's own `Display`, on a twin built the same way — and
    /// land in the variant that says where it came from.
    #[test]
    fn every_conversion_displays_exactly_what_it_wraps() {
        type Case = (
            fn() -> CliError,
            fn() -> String,
            fn(&CliError) -> bool,
            bool,
        );
        let cases: [Case; 9] = [
            (
                || std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied").into(),
                || std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied").to_string(),
                |e| matches!(e, CliError::Store(_)),
                true,
            ),
            (
                || herdrx::Error::Invalid("a bad token".into()).into(),
                || herdrx::Error::Invalid("a bad token".into()).to_string(),
                |e| matches!(e, CliError::Herdr(_)),
                true,
            ),
            (
                || String::from("owned words").into(),
                || "owned words".to_string(),
                |e| matches!(e, CliError::Message(_)),
                false,
            ),
            (
                || "".into(),
                String::new,
                |e| matches!(e, CliError::Message(_)),
                false,
            ),
            (
                || tower_mux::Error::NoWorkspace("dev".into()).into(),
                || tower_mux::Error::NoWorkspace("dev".into()).to_string(),
                |e| matches!(e, CliError::Other(_)),
                true,
            ),
            (
                || tower_wt::Error::NoTask.into(),
                || tower_wt::Error::NoTask.to_string(),
                |e| matches!(e, CliError::Other(_)),
                true,
            ),
            (
                || tower_cli::helpcmd::FlagError::Help.into(),
                || tower_cli::helpcmd::FlagError::Help.to_string(),
                |e| matches!(e, CliError::Other(_)),
                true,
            ),
            (
                || {
                    serde_json::from_str::<serde_json::Value>("{")
                        .unwrap_err()
                        .into()
                },
                || {
                    serde_json::from_str::<serde_json::Value>("{")
                        .unwrap_err()
                        .to_string()
                },
                |e| matches!(e, CliError::Other(_)),
                true,
            ),
            (
                || {
                    let boxed: Box<dyn Error> = Box::new(tower_wt::Error::NoTask);
                    boxed.into()
                },
                || tower_wt::Error::NoTask.to_string(),
                |e| matches!(e, CliError::Other(_)),
                true,
            ),
        ];
        for (make, twin, variant, has_source) in cases {
            let e = make();
            assert_eq!(e.to_string(), twin(), "{e:?}");
            assert!(variant(&e), "{e:?}");
            assert_eq!(e.source().is_some(), has_source, "{e:?}");
            if let Some(source) = e.source() {
                assert_eq!(source.to_string(), e.to_string(), "{e:?}");
            }
        }
    }

    /// Property and round trip: a message goes in as a `String` or a `&str`
    /// and reads back unchanged — empty, multi-line, non-ASCII or holding
    /// control characters — with no source behind it; the same words inside an
    /// `io::Error` read back unchanged too, now with that error behind them.
    #[test]
    fn a_message_reads_back_exactly_as_it_was_given() {
        const PIECES: [&str; 9] = ["", "a", " ", "\n", "\t", "é", "한글", "\u{1}", "{e}"];
        let mut rng = Lcg(0xe550);
        for _ in 0..500 {
            let words: String = (0..rng.below(8))
                .map(|_| PIECES[rng.below(PIECES.len() as u64) as usize])
                .collect();
            for e in [
                CliError::from(words.clone()),
                CliError::from(words.as_str()),
            ] {
                assert!(matches!(&e, CliError::Message(m) if *m == words), "{e:?}");
                assert_eq!(e.to_string(), words);
                assert!(e.source().is_none());
            }
            let store = CliError::from(std::io::Error::other(words.clone()));
            assert_eq!(store.to_string(), words);
            assert!(store.source().is_some());
        }
    }

    /// Round trip: the error that went in comes back out of `source`, and out
    /// of `Other`, by its own type — not a string copy and not a box around a
    /// box.
    #[test]
    fn the_wrapped_error_comes_back_out_by_its_own_type() {
        let store = CliError::from(std::io::Error::new(std::io::ErrorKind::StorageFull, "full"));
        let io = store
            .source()
            .and_then(|s| s.downcast_ref::<std::io::Error>())
            .expect("the io error itself");
        assert_eq!(io.kind(), std::io::ErrorKind::StorageFull);

        let herdr = CliError::from(herdrx::Error::NoSocket);
        assert!(matches!(
            herdr
                .source()
                .and_then(|s| s.downcast_ref::<herdrx::Error>()),
            Some(herdrx::Error::NoSocket)
        ));

        let json = CliError::from(serde_json::from_str::<u8>("300").unwrap_err());
        assert!(json
            .source()
            .and_then(|s| s.downcast_ref::<serde_json::Error>())
            .is_some());

        let boxed: Box<dyn Error> = Box::new(tower_wt::Error::NoTask);
        let e = CliError::from(boxed);
        let CliError::Other(inner) = &e else {
            panic!("{e:?} is not Other");
        };
        assert!(matches!(
            inner.downcast_ref::<tower_wt::Error>(),
            Some(tower_wt::Error::NoTask)
        ));
        assert!(matches!(
            e.source().and_then(|s| s.downcast_ref::<tower_wt::Error>()),
            Some(tower_wt::Error::NoTask)
        ));
    }
}

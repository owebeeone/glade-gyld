//! The environment this process STARTED with, captured once and handed down.
//!
//! **Read once, at the entry point.** `glade-gyld`'s `main` captures the whole
//! start-up environment into an [`Environment`] and passes it down through
//! [`crate::GyldConfig`], so nothing below the entry point reads the process's
//! own (ProcessGlobalsPlan Step 2.2). The agent's endpoint and model
//! ([`crate::agent::resolve`]), the model key ([`crate::model::discover_key`]),
//! the key check each request makes and the GitHub token
//! ([`crate::github::discover`]) all read this snapshot. A test hands a supplier
//! exactly the variables it made up, and the default is none at all.
//!
//! **It holds secrets, and it never prints one.** An API key and a GitHub token
//! are ordinary variables here, so [`Environment`]'s `Debug` is hand-written to
//! print the NAMES and never a value, and there is no other way out of it: no
//! `Display` and no serialisation.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::sync::Arc;

/// A snapshot of a process environment: every name and value, as captured.
///
/// Immutable once made, and cheap to clone: the one map is shared, so the
/// supplier can carry it onto every task that reads a variable.
#[derive(Clone, Default)]
pub struct Environment {
    vars: Arc<BTreeMap<OsString, OsString>>,
}

impl Environment {
    /// A snapshot of `vars`: the entry point passes `std::env::vars_os()`, and a
    /// test passes the variables it made up.
    ///
    /// The FIRST of a repeated name wins, which is what `getenv` answers for an
    /// environment that repeats one.
    pub fn of<I, K, V>(vars: I) -> Environment
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        let mut held = BTreeMap::new();
        for (name, value) in vars {
            held.entry(name.into()).or_insert_with(|| value.into());
        }
        Environment {
            vars: Arc::new(held),
        }
    }

    /// The variable `name`, as `std::env::var(name).ok()` said at capture: its
    /// value when it is set and unicode, and `None` otherwise. What a blank
    /// value means is the reader's business.
    pub fn var(&self, name: &str) -> Option<String> {
        self.vars
            .get(OsStr::new(name))
            .and_then(|value| value.to_str())
            .map(str::to_string)
    }
}

/// Hand-written, and that is the point: a `{:?}` on this, or on a config that
/// holds it, prints which variables there are and never what one says.
impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment")
            .field("names", &self.vars.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_variable_is_its_captured_value_and_an_absent_one_is_none() {
        let env = Environment::of([("GYLD_MADE_UP_ONE", "one"), ("GYLD_MADE_UP_BLANK", "")]);
        assert_eq!(env.var("GYLD_MADE_UP_ONE").as_deref(), Some("one"));
        assert_eq!(
            env.var("GYLD_MADE_UP_BLANK").as_deref(),
            Some(""),
            "set and empty; the reader decides what blank means"
        );
        assert_eq!(env.var("GYLD_MADE_UP_TWO"), None);
    }

    /// Asserted without printing what came back: were the process read after
    /// all, the value would be a real one.
    #[test]
    fn only_the_snapshot_is_read_and_never_the_process_environment() {
        assert!(
            std::env::var_os("PATH").is_some(),
            "this process has a PATH, which is what makes the rest a test"
        );
        assert!(
            Environment::default().var("PATH").is_none(),
            "the default is no variables at all"
        );
        let env = Environment::of([("GYLD_MADE_UP_ONE", "one")]);
        assert!(
            env.var("PATH").is_none(),
            "a name the snapshot lacks is unset, whatever the process holds"
        );
    }

    #[test]
    fn the_first_of_a_repeated_name_wins_as_getenv_answers() {
        let env = Environment::of([("GYLD_MADE_UP", "first"), ("GYLD_MADE_UP", "second")]);
        assert_eq!(env.var("GYLD_MADE_UP").as_deref(), Some("first"));
    }

    #[test]
    fn debug_names_the_variables_and_prints_no_value() {
        let env = Environment::of([("GYLD_MADE_UP_SECRET", "made-up-secret-value")]);
        for printed in [format!("{env:?}"), format!("{env:#?}")] {
            assert!(
                printed.contains("GYLD_MADE_UP_SECRET"),
                "the name is printable: {printed}"
            );
            assert!(
                !printed.contains("made-up-secret-value"),
                "a value is not: {printed}"
            );
        }
    }

    /// A value that is not unicode reads as unset, as `std::env::var` answers
    /// `NotUnicode`: a key or a token that is not text is no key and no token.
    #[cfg(unix)]
    mod unix {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        use super::super::Environment;

        #[test]
        fn a_value_that_is_not_unicode_reads_as_unset() {
            let env = Environment::of([(
                OsString::from("GYLD_MADE_UP"),
                OsString::from_vec(vec![b'x', 0xff]),
            )]);
            assert_eq!(env.var("GYLD_MADE_UP"), None);
        }
    }
}

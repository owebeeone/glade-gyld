use super::*;

/// The model key, at the moment of the call and nowhere else.
///
/// `ANTHROPIC_API_KEY` in the environment the supplier started with (`env`,
/// captured at its entry point) first, then [`crate::agent::AUTH_TOKEN_ENV`],
/// then the key file. The second name is there because it is the one
/// Claude-shaped clients and the dabeest launchers already export, and a local
/// endpoint's token is a dummy value that must nonetheless be present. The file
/// is MODE CHECKED: a credential any other account on the machine can read is
/// refused rather than used, because a supplier that quietly accepts one
/// teaches everybody that it is fine.
pub fn discover_key(env: &Environment, key_file: &Path) -> Result<String, AskRefusal> {
    for name in [KEY_ENV, crate::agent::AUTH_TOKEN_ENV] {
        if let Some(value) = env.var(name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }
    let read = std::fs::read_to_string(key_file);
    let text = match read {
        Ok(text) => text,
        Err(_) => {
            return Err(AskRefusal::NoModelKey {
                key_file: key_file.to_path_buf(),
            });
        }
    };
    platform::check_mode(key_file)?;
    let key = text.lines().next().unwrap_or("").trim().to_string();
    if key.is_empty() {
        return Err(AskRefusal::NoModelKey {
            key_file: key_file.to_path_buf(),
        });
    }
    Ok(key)
}

/// The key file's mode check, behind an explicit platform boundary (the
/// workzone's conditional-compilation rule: no bare `#[cfg]` on a declaration).
///
/// `pub(crate)` because the search key gets the SAME check
/// ([`crate::websearch::SearchKey`]): a second, slightly different rule about
/// who may read a credential would be a rule nobody could state.
#[cfg(unix)]
pub(crate) mod platform {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use crate::ask::AskRefusal;

    /// Refuse a key file any group or other account can read.
    pub fn check_mode(key_file: &Path) -> Result<(), AskRefusal> {
        let mode = match std::fs::metadata(key_file) {
            Ok(meta) => meta.permissions().mode() & 0o777,
            Err(_) => {
                return Err(AskRefusal::NoModelKey {
                    key_file: key_file.to_path_buf(),
                });
            }
        };
        if mode & 0o077 != 0 {
            return Err(AskRefusal::KeyFileMode {
                key_file: key_file.to_path_buf(),
                mode,
            });
        }
        Ok(())
    }
}

#[cfg(not(unix))]
pub(crate) mod platform {
    use std::path::Path;

    use crate::ask::AskRefusal;

    /// No POSIX mode to check here; the file's own ACL is the platform's.
    pub fn check_mode(_key_file: &Path) -> Result<(), AskRefusal> {
        Ok(())
    }
}

/// Strip anything that could be a credential out of a transport error before it
/// becomes data. A reqwest error names the URL, never a header, but a redacted
/// query is cheaper than trusting that forever.
///
/// `pub(crate)` because the network tools of section 11.7 report their own
/// transport failures as data and must redact them the same way.
pub(crate) fn scrub(said: &str) -> String {
    said.split_whitespace()
        .map(|word| match word.split_once('?') {
            Some((head, _)) => format!("{head}?…"),
            None => word.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

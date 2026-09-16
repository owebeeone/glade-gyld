//! How the ask agent is configured when nobody can pass it a flag.
//!
//! **The supplier is spawned with a fixed argument list.** grazel composes
//! `glade-gyld`'s argv itself (`grazel/src/lib.rs`, `gyld_supplier_argv`) and
//! passes none of the `--agent-*` flags through, and the owner starts grazel
//! from `gyld-ui.py`. So the flags are the one channel that CANNOT reach a
//! running desk. Two that can: the environment grazel inherits and hands on,
//! and a file in the app-owned bundle root beside the key.
//!
//! Three sources, in increasing authority:
//!
//! 1. `<bundle-root>/agent/config.json` — the app's own settings, read at
//!    attach and again at every call, so a model can be changed without
//!    restarting anything. Every field is optional.
//! 2. The environment: [`BASE_URL_ENV`], [`MODEL_ENV`], and the two key
//!    variables. [`AUTH_TOKEN_ENV`] is here because it is what the dabeest
//!    client guide sets and what Claude-shaped clients already export.
//! 3. The flags, for a supplier somebody CAN pass flags to.
//!
//! **A value nobody set is not a value.** Everything here is `Option`, and the
//! defaults are applied once at the end — after the profile is known, because
//! the sensible output budget for a thinking model on a 96K window is not
//! Anthropic's. A flag whose default beat a config file would make the file
//! unusable, which is the whole reason this module is not a `ModelConfig` with
//! defaults in it.
//!
//! **No key value is ever read here.** The key file's PATH is configuration;
//! the key itself is read by the model client at the moment of the call
//! ([`crate::model::discover_key`]) and by nothing else.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::model::{ModelConfig, Shape, DEFAULT_AGENT_MODEL, DEFAULT_BASE_URL};

/// The endpoint, when the environment names one. The name Anthropic-shaped
/// clients already use, including the dabeest launchers.
pub const BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";

/// The model, when the environment names one.
///
/// This one is ours, not Anthropic's: `ANTHROPIC_MODEL` is Claude Code's own
/// variable and would be read here by accident on any desk that has it set for
/// a different client. A supplier that silently inherited another program's
/// model choice is the surprise this name avoids.
pub const MODEL_ENV: &str = "GYLD_AGENT_MODEL";

/// The compatibility profile, when the environment names one.
pub const COMPAT_ENV: &str = "GYLD_AGENT_COMPAT";

/// The second key source, checked after [`crate::ask::KEY_ENV`]. Local
/// endpoints want a token that is ignored but must be present, and this is the
/// name the dabeest guide sets it under.
pub const AUTH_TOKEN_ENV: &str = "ANTHROPIC_AUTH_TOKEN";

/// The config file, relative to the app-owned bundle root — beside the key
/// file, in the one directory the app already owns.
pub const DEFAULT_CONFIG_FILE: &str = "agent/config.json";

/// Which dialect of the Messages API the endpoint speaks.
///
/// Not a vendor list and not a feature flag: it is the answer to "what may I
/// assume is there?". Anthropic's own endpoint has `count_tokens`, honours
/// `strict` on a tool and `cache_control` on a block, and authenticates with
/// `x-api-key`. A patched Ollama serving the same API has none of the first
/// three and wants a bearer token. Everything this profile decides is a
/// STARTING assumption; the client still degrades on what the endpoint
/// actually says, and says so.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Compat {
    /// The Messages API as Anthropic serves it. The default, and unchanged.
    #[default]
    Anthropic,
    /// The Messages API as Ollama 0.14+ serves it (`/v1/messages`, streaming,
    /// tool use, thinking blocks) — with no `/v1/messages/count_tokens`.
    Ollama,
}

impl Compat {
    pub fn name(self) -> &'static str {
        match self {
            Compat::Anthropic => "anthropic",
            Compat::Ollama => "ollama",
        }
    }

    /// Read a profile by name. An unknown name is an error rather than a
    /// silent fall-back to the default: a misspelt profile that quietly became
    /// `anthropic` would send `count_tokens` at an endpoint that 404s it and
    /// blame the endpoint.
    pub fn parse(text: &str) -> Result<Compat, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Ok(Compat::Anthropic),
            "ollama" => Ok(Compat::Ollama),
            other => Err(format!(
                "unknown compat profile {other:?}: it is \"anthropic\" or \"ollama\""
            )),
        }
    }

    /// The profile a base URL implies when nobody named one.
    ///
    /// Anthropic's own hosts keep the Anthropic profile; anything else is
    /// assumed to be a local or third-party endpoint. This is what lets a desk
    /// be pointed at dabeest with one environment variable and no config file
    /// at all — and naming `compat` explicitly always wins over it.
    pub fn detect(base_url: &str) -> Compat {
        let host = host_of(base_url);
        if host == "anthropic.com" || host.ends_with(".anthropic.com") {
            return Compat::Anthropic;
        }
        Compat::Ollama
    }

    /// Whether the endpoint is asked to count the input before the call.
    ///
    /// Ollama has no `/v1/messages/count_tokens` — the dabeest guide says so in
    /// its endpoint table — so the profile skips it rather than spending a
    /// round trip discovering the 404 on every single turn.
    pub fn counts_tokens(self) -> bool {
        matches!(self, Compat::Anthropic)
    }

    /// Whether the key also travels as `Authorization: Bearer`.
    ///
    /// Claude-shaped clients authenticate to these endpoints with a bearer
    /// token (`ANTHROPIC_AUTH_TOKEN`), so both headers are sent: the value is
    /// the same one, and an endpoint that reads either is satisfied.
    pub fn sends_bearer(self) -> bool {
        matches!(self, Compat::Ollama)
    }

    /// The request's `max_tokens` when nobody set one.
    ///
    /// **Headroom, because these are thinking models.** The dabeest guide is
    /// explicit: thought tokens count against `max_tokens`, so a small cap
    /// returns empty content with the answer never emitted. Its own launchers
    /// export `CLAUDE_CODE_MAX_OUTPUT_TOKENS=32768` and warn against lowering
    /// it below 32000, so that is the number here.
    pub fn default_max_output_tokens(self) -> u64 {
        match self {
            Compat::Anthropic => crate::model::DEFAULT_MAX_OUTPUT_TOKENS,
            Compat::Ollama => 32_768,
        }
    }

    /// The per-run input budget when nobody set one.
    ///
    /// The daily driver `qwen3.8-96k` has a 96K window (98,304 tokens) and the
    /// output budget above has to fit inside it beside the input, so the input
    /// gets what is left: 98,304 − 32,768 = 65,536. A budget larger than the
    /// window is not a budget — Ollama would silently truncate instead.
    pub fn default_max_input_tokens(self) -> u64 {
        match self {
            Compat::Anthropic => crate::model::DEFAULT_MAX_INPUT_TOKENS,
            Compat::Ollama => 65_536,
        }
    }

    /// The tool and cache shape the FIRST request of a run is sent in.
    ///
    /// Full on both profiles, deliberately. `strict` and `cache_control` are
    /// worth having wherever they work, and an endpoint that rejects one says
    /// so in a 400 the client reads and retries without — which is a fact about
    /// the endpoint, discovered, rather than a guess baked into a profile. The
    /// config file can still start either one off.
    pub fn default_shape(self) -> Shape {
        Shape::full()
    }
}

/// The host of a base URL, lowercased and without a port — enough to tell
/// Anthropic's endpoint from anything else, and nothing more.
fn host_of(base_url: &str) -> String {
    let rest = match base_url.split_once("://") {
        Some((_, rest)) => rest,
        None => base_url,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    };
    let host = match authority.rsplit_once(':') {
        // An IPv6 literal has colons of its own; it is never Anthropic's host.
        Some((head, _)) if !head.contains(':') => head,
        _ => authority,
    };
    host.trim_matches(['[', ']']).to_ascii_lowercase()
}

/// Everything one source has to say. A field nobody set is `None` and stays
/// `None`: that is what makes the three sources composable.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentOverrides {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub compat: Option<Compat>,
    /// The request's own `max_tokens` — the output budget.
    pub max_tokens: Option<u64>,
    pub max_input_tokens: Option<u64>,
    pub max_conversation_tokens: Option<u64>,
    pub timeout_secs: Option<u64>,
    pub key_file: Option<PathBuf>,
    /// Start with `strict` off on the draft tool rather than discovering it.
    pub strict: Option<bool>,
    /// Start with no `cache_control` breakpoints rather than discovering it.
    pub cache_control: Option<bool>,
    /// Count the input with the endpoint before the call, or don't.
    pub count_tokens: Option<bool>,
}

impl AgentOverrides {
    /// `higher` over `self`: every field `higher` set wins, and every field it
    /// left alone keeps whatever `self` had. This is the whole precedence rule,
    /// and it is applied twice — file, then environment, then flags.
    pub fn under(self, higher: &AgentOverrides) -> AgentOverrides {
        AgentOverrides {
            model: higher.model.clone().or(self.model),
            base_url: higher.base_url.clone().or(self.base_url),
            compat: higher.compat.or(self.compat),
            max_tokens: higher.max_tokens.or(self.max_tokens),
            max_input_tokens: higher.max_input_tokens.or(self.max_input_tokens),
            max_conversation_tokens: higher
                .max_conversation_tokens
                .or(self.max_conversation_tokens),
            timeout_secs: higher.timeout_secs.or(self.timeout_secs),
            key_file: higher.key_file.clone().or(self.key_file),
            strict: higher.strict.or(self.strict),
            cache_control: higher.cache_control.or(self.cache_control),
            count_tokens: higher.count_tokens.or(self.count_tokens),
        }
    }

    /// What the environment says. Only the three names a desk can realistically
    /// set are read, plus the profile: the numbers belong in the config file,
    /// where they can carry a comment and a reason.
    ///
    /// An empty or blank variable is treated as unset. `ANTHROPIC_BASE_URL=`
    /// in a launcher script is how a variable gets cleared, and reading it as
    /// "the endpoint is the empty string" would break the run instead.
    pub fn from_env() -> (AgentOverrides, Vec<String>) {
        AgentOverrides::from_vars(&|name| std::env::var(name).ok())
    }

    /// [`AgentOverrides::from_env`] over an arbitrary lookup, so the precedence
    /// is tested without touching the process environment.
    pub fn from_vars(var: &dyn Fn(&str) -> Option<String>) -> (AgentOverrides, Vec<String>) {
        let mut notes: Vec<String> = Vec::new();
        let read = |name: &str| -> Option<String> {
            let value = var(name)?;
            let value = value.trim().to_string();
            if value.is_empty() {
                return None;
            }
            Some(value)
        };
        let compat = match read(COMPAT_ENV) {
            Some(text) => match Compat::parse(&text) {
                Ok(profile) => Some(profile),
                Err(reason) => {
                    notes.push(format!("{COMPAT_ENV} is ignored: {reason}"));
                    None
                }
            },
            None => None,
        };
        (
            AgentOverrides {
                model: read(MODEL_ENV),
                base_url: read(BASE_URL_ENV),
                compat,
                ..Default::default()
            },
            notes,
        )
    }

    /// What the config file says, and everything that was wrong with it.
    ///
    /// **An unreadable or malformed file is a NOTE, not a refusal.** The desk
    /// still has an environment and a set of defaults, and a supplier that
    /// refused to attach over a stray comma would take the whole app down for
    /// a file the app itself is allowed to have none of. A file that is simply
    /// absent is the ordinary case and says nothing at all.
    pub fn from_file(path: &Path) -> (AgentOverrides, Vec<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (AgentOverrides::default(), Vec::new());
            }
            Err(e) => {
                return (
                    AgentOverrides::default(),
                    vec![format!("{} could not be read: {e}", path.display())],
                );
            }
        };
        AgentOverrides::from_json(&text, path)
    }

    /// Read one config document. PURE, so the whole file vocabulary — every
    /// field, a misspelt one, a bad profile name — is asserted with no
    /// filesystem in sight.
    pub fn from_json(text: &str, path: &Path) -> (AgentOverrides, Vec<String>) {
        let mut notes: Vec<String> = Vec::new();
        let held: AgentFile = match serde_json::from_str(text) {
            Ok(held) => held,
            Err(e) => {
                return (
                    AgentOverrides::default(),
                    vec![format!("{} did not decode: {e}", path.display())],
                );
            }
        };
        // A misspelt field is NAMED. serde would drop it in silence, and a
        // config file whose setting does nothing and says nothing is the worst
        // of the three outcomes.
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(text) {
            for key in map.keys() {
                if !AgentFile::FIELDS.contains(&key.as_str()) {
                    notes.push(format!(
                        "{} names {key:?}, which is not a setting; it is ignored",
                        path.display()
                    ));
                }
            }
        }
        let compat = match held.compat.as_deref() {
            Some(text) => match Compat::parse(text) {
                Ok(profile) => Some(profile),
                Err(reason) => {
                    notes.push(format!("{}: {reason}", path.display()));
                    None
                }
            },
            None => None,
        };
        (
            AgentOverrides {
                model: held.model,
                base_url: held.base_url,
                compat,
                max_tokens: held.max_tokens,
                max_input_tokens: held.max_input_tokens,
                max_conversation_tokens: held.max_conversation_tokens,
                timeout_secs: held.timeout_secs,
                key_file: held.key_file.map(PathBuf::from),
                strict: held.strict,
                cache_control: held.cache_control,
                count_tokens: held.count_tokens,
            },
            notes,
        )
    }
}

/// `agent/config.json` itself. Every field optional, and the names are the
/// ones the README documents.
#[derive(Clone, Debug, Default, Deserialize)]
struct AgentFile {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    compat: Option<String>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    max_input_tokens: Option<u64>,
    #[serde(default)]
    max_conversation_tokens: Option<u64>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    key_file: Option<String>,
    #[serde(default)]
    strict: Option<bool>,
    #[serde(default)]
    cache_control: Option<bool>,
    #[serde(default)]
    count_tokens: Option<bool>,
}

impl AgentFile {
    /// Every settable name, for the misspelling note. Kept beside the struct
    /// because a field added there and forgotten here would be reported as a
    /// mistake the moment somebody used it.
    const FIELDS: &'static [&'static str] = &[
        "model",
        "base_url",
        "compat",
        "max_tokens",
        "max_input_tokens",
        "max_conversation_tokens",
        "timeout_secs",
        "key_file",
        "strict",
        "cache_control",
        "count_tokens",
    ];
}

/// The configuration a turn is actually made with, and everything resolving it
/// had to say.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    pub config: ModelConfig,
    /// Notes about the CONFIGURATION — a file that did not decode, a setting
    /// nobody has heard of. They reach the run's records like every other
    /// fallback, so a desk being served by the wrong model can find out why.
    pub notes: Vec<String>,
}

impl Resolved {
    /// The one line a log carries at attach: the endpoint, the model and the
    /// profile. Never the key, and never the key's presence either — that is
    /// the refusal's business.
    pub fn says(&self) -> String {
        format!(
            "base-url {} model {} compat {} (max_tokens {}, max input {})",
            self.config.base_url,
            self.config.model,
            self.config.compat.name(),
            self.config.max_output_tokens,
            self.config.max_input_tokens,
        )
    }
}

/// Resolve the three sources into one configuration.
///
/// The defaults come LAST and depend on the profile, which depends on the base
/// URL: a local endpoint's output budget is not Anthropic's, and neither is the
/// window its input has to fit in. `bundle_root` is what a relative `key_file`
/// and the config file itself are resolved against — the app's own directory,
/// never a requester's.
pub fn resolve(bundle_root: &Path, flags: &AgentOverrides) -> Resolved {
    let path = bundle_root.join(DEFAULT_CONFIG_FILE);
    let (file, mut notes) = AgentOverrides::from_file(&path);
    let (env, env_notes) = AgentOverrides::from_env();
    notes.extend(env_notes);
    resolve_from(bundle_root, file.under(&env).under(flags), notes)
}

/// [`resolve`] once the three sources are already merged, so precedence is
/// tested apart from where each source came from.
pub fn resolve_from(bundle_root: &Path, merged: AgentOverrides, notes: Vec<String>) -> Resolved {
    let base_url = merged
        .base_url
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
        .trim_end_matches('/')
        .to_string();
    let compat = merged.compat.unwrap_or_else(|| Compat::detect(&base_url));
    let key_file = match merged.key_file {
        Some(path) if path.is_absolute() => path,
        Some(path) => bundle_root.join(path),
        None => bundle_root.join(crate::ask::DEFAULT_KEY_FILE),
    };
    let shape = Shape {
        strict: merged.strict.unwrap_or(compat.default_shape().strict),
        cache_control: merged
            .cache_control
            .unwrap_or(compat.default_shape().cache_control),
    };
    Resolved {
        config: ModelConfig {
            model: merged
                .model
                .unwrap_or_else(|| DEFAULT_AGENT_MODEL.to_string()),
            base_url,
            compat,
            max_input_tokens: merged
                .max_input_tokens
                .unwrap_or_else(|| compat.default_max_input_tokens()),
            max_output_tokens: merged
                .max_tokens
                .unwrap_or_else(|| compat.default_max_output_tokens()),
            max_conversation_tokens: merged
                .max_conversation_tokens
                .unwrap_or(crate::model::DEFAULT_MAX_CONVERSATION_TOKENS),
            timeout: Duration::from_secs(
                merged
                    .timeout_secs
                    .unwrap_or(crate::model::DEFAULT_MODEL_TIMEOUT_SECS),
            ),
            key_file,
            count_tokens: merged.count_tokens.unwrap_or(compat.counts_tokens()),
            shape,
        },
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle() -> PathBuf {
        PathBuf::from("/tmp/bundle")
    }

    fn resolved(merged: AgentOverrides) -> ModelConfig {
        resolve_from(&bundle(), merged, Vec::new()).config
    }

    #[test]
    fn nothing_configured_is_the_anthropic_path_exactly_as_it_was() {
        let config = resolved(AgentOverrides::default());
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.model, DEFAULT_AGENT_MODEL);
        assert_eq!(config.compat, Compat::Anthropic);
        assert!(config.count_tokens, "the endpoint counts, as it always did");
        assert_eq!(config.shape, Shape::full());
        assert_eq!(
            config.max_output_tokens,
            crate::model::DEFAULT_MAX_OUTPUT_TOKENS
        );
        assert_eq!(
            config.max_input_tokens,
            crate::model::DEFAULT_MAX_INPUT_TOKENS
        );
        assert_eq!(config.key_file, bundle().join("agent/api-key"));
        assert_eq!(
            config,
            ModelConfig::default_at(&bundle()),
            "the resolved default IS the default"
        );
    }

    #[test]
    fn a_base_url_that_is_not_anthropics_picks_the_ollama_profile_and_its_budgets() {
        let config = resolved(AgentOverrides {
            base_url: Some("http://127.0.0.1:11434".into()),
            ..Default::default()
        });
        assert_eq!(config.compat, Compat::Ollama);
        assert!(
            !config.count_tokens,
            "ollama has no /v1/messages/count_tokens"
        );
        assert_eq!(config.max_output_tokens, 32_768, "headroom for thinking");
        assert_eq!(config.max_input_tokens, 65_536, "96K minus the output");
        assert_eq!(
            config.shape,
            Shape::full(),
            "strict and cache_control are tried, then discovered"
        );

        // Naming the profile always wins over the detection.
        let forced = resolved(AgentOverrides {
            base_url: Some("http://127.0.0.1:11434".into()),
            compat: Some(Compat::Anthropic),
            ..Default::default()
        });
        assert_eq!(forced.compat, Compat::Anthropic);
        assert!(forced.count_tokens);
    }

    #[test]
    fn anthropics_own_hosts_keep_the_anthropic_profile_and_everything_else_does_not() {
        for url in [
            "https://api.anthropic.com",
            "https://API.Anthropic.com/",
            "https://anthropic.com",
            "https://eu.api.anthropic.com/v1",
        ] {
            assert_eq!(Compat::detect(url), Compat::Anthropic, "{url}");
        }
        for url in [
            "http://127.0.0.1:11434",
            "http://localhost:11434",
            "https://api.anthropic.com.example.net",
            "http://[::1]:11434",
            "",
        ] {
            assert_eq!(Compat::detect(url), Compat::Ollama, "{url}");
        }
    }

    #[test]
    fn a_trailing_slash_on_the_base_url_is_not_a_second_one_in_the_path() {
        let config = resolved(AgentOverrides {
            base_url: Some("http://127.0.0.1:11434/".into()),
            ..Default::default()
        });
        assert_eq!(config.base_url, "http://127.0.0.1:11434");
    }

    #[test]
    fn the_file_is_read_the_environment_beats_it_and_the_flags_beat_both() {
        let file = AgentOverrides {
            base_url: Some("http://file:1".into()),
            model: Some("from-file".into()),
            max_tokens: Some(111),
            ..Default::default()
        };
        let (env, notes) = AgentOverrides::from_vars(&|name| match name {
            BASE_URL_ENV => Some("http://env:2".into()),
            MODEL_ENV => Some("from-env".into()),
            _ => None,
        });
        assert!(notes.is_empty(), "{notes:?}");
        let flags = AgentOverrides {
            model: Some("from-flag".into()),
            ..Default::default()
        };

        // File alone.
        let only_file = resolved(file.clone());
        assert_eq!(only_file.base_url, "http://file:1");
        assert_eq!(only_file.model, "from-file");

        // The environment beats the file where it speaks, and only there.
        let over = resolved(file.clone().under(&env));
        assert_eq!(over.base_url, "http://env:2");
        assert_eq!(over.model, "from-env");
        assert_eq!(over.max_output_tokens, 111, "the file still holds the rest");

        // The flags beat both, and again only where they speak.
        let all = resolved(file.under(&env).under(&flags));
        assert_eq!(all.model, "from-flag");
        assert_eq!(all.base_url, "http://env:2");
        assert_eq!(all.max_output_tokens, 111);
    }

    #[test]
    fn a_blank_environment_variable_is_not_a_value() {
        let (env, notes) = AgentOverrides::from_vars(&|name| match name {
            BASE_URL_ENV => Some("   ".into()),
            MODEL_ENV => Some("".into()),
            _ => None,
        });
        assert_eq!(env, AgentOverrides::default(), "blank is unset");
        assert!(notes.is_empty());

        let (env, notes) = AgentOverrides::from_vars(&|name| match name {
            COMPAT_ENV => Some("ollamaa".into()),
            _ => None,
        });
        assert_eq!(env.compat, None);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("ollamaa") && notes[0].contains(COMPAT_ENV));
    }

    #[test]
    fn every_documented_field_of_the_config_file_is_read() {
        let text = r#"{
            "base_url": "http://127.0.0.1:11434",
            "model": "qwen3.8-96k",
            "compat": "ollama",
            "max_tokens": 32768,
            "max_input_tokens": 60000,
            "max_conversation_tokens": 300000,
            "timeout_secs": 900,
            "key_file": "agent/other-key",
            "strict": false,
            "cache_control": false,
            "count_tokens": false
        }"#;
        let (held, notes) = AgentOverrides::from_json(text, Path::new("config.json"));
        assert!(notes.is_empty(), "{notes:?}");
        let config = resolved(held);
        assert_eq!(config.base_url, "http://127.0.0.1:11434");
        assert_eq!(config.model, "qwen3.8-96k");
        assert_eq!(config.compat, Compat::Ollama);
        assert_eq!(config.max_output_tokens, 32_768);
        assert_eq!(config.max_input_tokens, 60_000);
        assert_eq!(config.max_conversation_tokens, 300_000);
        assert_eq!(config.timeout, Duration::from_secs(900));
        assert_eq!(config.key_file, bundle().join("agent/other-key"));
        assert_eq!(
            config.shape,
            Shape {
                strict: false,
                cache_control: false
            },
            "the profile can start degraded instead of discovering it"
        );
        assert!(!config.count_tokens);

        // An absolute key file is taken as it stands.
        let (held, _) = AgentOverrides::from_json(r#"{"key_file": "/etc/k"}"#, Path::new("c"));
        assert_eq!(resolved(held).key_file, PathBuf::from("/etc/k"));
    }

    #[test]
    fn an_empty_document_configures_nothing_and_says_nothing() {
        let (held, notes) = AgentOverrides::from_json("{}", Path::new("config.json"));
        assert_eq!(held, AgentOverrides::default());
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn a_broken_file_a_misspelt_setting_and_a_bad_profile_are_notes_not_refusals() {
        let (held, notes) = AgentOverrides::from_json("{oops", Path::new("config.json"));
        assert_eq!(held, AgentOverrides::default(), "nothing is taken from it");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("did not decode"), "{notes:?}");

        let (held, notes) = AgentOverrides::from_json(
            r#"{"model": "m", "max_token": 10, "compat": "ollamaa"}"#,
            Path::new("config.json"),
        );
        assert_eq!(
            held.model.as_deref(),
            Some("m"),
            "the good field still lands"
        );
        assert_eq!(held.compat, None);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(
            notes.iter().any(|n| n.contains("\"max_token\"")),
            "a misspelt setting is NAMED: {notes:?}"
        );
        assert!(notes.iter().any(|n| n.contains("ollamaa")), "{notes:?}");
    }

    #[test]
    fn an_absent_file_is_the_ordinary_case_and_an_unreadable_one_is_a_note() {
        let dir = std::env::temp_dir().join(format!("glade-gyld-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("agent")).unwrap();

        let (held, notes) = AgentOverrides::from_file(&dir.join(DEFAULT_CONFIG_FILE));
        assert_eq!(held, AgentOverrides::default());
        assert!(notes.is_empty(), "an absent file says nothing: {notes:?}");

        // A DIRECTORY where the file should be is not "not found".
        std::fs::create_dir_all(dir.join(DEFAULT_CONFIG_FILE)).unwrap();
        let (_, notes) = AgentOverrides::from_file(&dir.join(DEFAULT_CONFIG_FILE));
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("could not be read"), "{notes:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_attach_line_names_the_endpoint_and_the_model_and_no_key() {
        let held = Resolved {
            config: resolved(AgentOverrides {
                base_url: Some("http://127.0.0.1:11434".into()),
                model: Some("qwen3.8-96k".into()),
                ..Default::default()
            }),
            notes: Vec::new(),
        };
        let said = held.says();
        assert!(said.contains("http://127.0.0.1:11434"), "{said}");
        assert!(said.contains("qwen3.8-96k"), "{said}");
        assert!(said.contains("compat ollama"), "{said}");
        assert!(!said.contains("key"), "a key is in no log line: {said}");
    }
}

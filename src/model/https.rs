use super::*;

/// The API version header every request carries.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// What this client has LEARNED about the endpoint it is talking to.
///
/// A degradation discovered once is remembered, so a conversation does not pay
/// two wasted round trips on every single turn to rediscover that the endpoint
/// dislikes `strict`. It only ever narrows — nothing here turns a feature back
/// on — except when the endpoint itself changes, which is possible because the
/// configuration is re-read at every call: a fact learned about one endpoint is
/// no fact at all about the next, so it is dropped with the base URL it was
/// learned for.
#[derive(Debug, Default)]
struct Learned {
    base_url: String,
    no_strict: bool,
    no_cache_control: bool,
    no_count_tokens: bool,
}

/// The real client: raw HTTPS to the Messages API, over rustls.
pub struct HttpsModelClient {
    config: ModelConfig,
    /// The environment the supplier started with: where the key is looked for
    /// before the key file ([`discover_key`]).
    env: Environment,
    http: OnceLock<reqwest::blocking::Client>,
    learned: std::sync::Mutex<Learned>,
}

/// A non-2xx answer, as data: the status is what decides whether a retry is
/// even worth attempting, so it does not get folded into the message first.
struct Rejected {
    status: u16,
    said: String,
}

impl Rejected {
    fn says(&self) -> String {
        format!("the model answered {}: {}", self.status, self.said)
    }
}

impl HttpsModelClient {
    /// A client for `config`, taking its key from `env` — the snapshot the entry
    /// point captured — before the key file.
    pub fn new(config: ModelConfig, env: Environment) -> HttpsModelClient {
        HttpsModelClient {
            config,
            env,
            http: OnceLock::new(),
            learned: std::sync::Mutex::new(Learned::default()),
        }
    }

    pub fn config(&self) -> &ModelConfig {
        &self.config
    }

    /// The learned state for THIS endpoint, reset when the endpoint changes.
    fn learned(&self, base_url: &str) -> std::sync::MutexGuard<'_, Learned> {
        let mut held = self
            .learned
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if held.base_url != base_url {
            *held = Learned {
                base_url: base_url.to_string(),
                ..Default::default()
            };
        }
        held
    }

    /// The shape this call STARTS in: what the configuration asks for, minus
    /// everything this endpoint has already refused.
    fn starting_shape(&self, config: &ModelConfig) -> Shape {
        let learned = self.learned(&config.base_url);
        Shape {
            strict: config.shape.strict && !learned.no_strict,
            cache_control: config.shape.cache_control && !learned.no_cache_control,
        }
    }

    /// Remember a rejection, so the next turn does not rediscover it.
    fn remember(&self, config: &ModelConfig, shape: Shape) {
        let mut learned = self.learned(&config.base_url);
        learned.no_strict = learned.no_strict || !shape.strict;
        learned.no_cache_control = learned.no_cache_control || !shape.cache_control;
    }

    /// The blocking HTTP client, built on FIRST USE and not before.
    ///
    /// Deliberately lazy. A blocking HTTP client must not be built or called
    /// from inside an async context — it asserts that in debug builds — and
    /// every call this client makes is on a `spawn_blocking` task. Building it
    /// where it is used keeps that true by construction rather than by a note:
    /// building it at attach, on the async path, panicked the supplier at
    /// start-up.
    fn http(&self) -> Result<&reqwest::blocking::Client, String> {
        if let Some(held) = self.http.get() {
            return Ok(held);
        }
        let built = reqwest::blocking::Client::builder()
            .timeout(self.config.timeout)
            .build()
            .map_err(|e| format!("cannot build the model client: {e}"))?;
        let _ = self.http.set(built);
        self.http
            .get()
            .ok_or_else(|| "the model client vanished after it was built".to_string())
    }

    /// One request, with the key read at this moment and dropped when it
    /// returns. A non-2xx answer carries the API's own message as data.
    ///
    /// The key travels as `x-api-key` always, and ALSO as
    /// `Authorization: Bearer` under a profile that wants it: local endpoints
    /// serving this API authenticate the way Claude-shaped clients do, the
    /// value is the same one either way, and an endpoint that reads either
    /// header is satisfied by one request rather than by a probe.
    ///
    /// `config` is the REQUEST's, not the client's: the configuration is
    /// re-read at every call, so the endpoint this goes to is the one the
    /// caller resolved, never the one attach happened to see.
    fn post(
        &self,
        config: &ModelConfig,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::blocking::Response, Rejected> {
        let key = discover_key(&self.env, &config.key_file).map_err(|r| Rejected {
            status: 0,
            said: r.says(),
        })?;
        let mut post = self
            .http()
            .map_err(|said| Rejected { status: 0, said })?
            .post(format!("{}{path}", config.base_url))
            .header("x-api-key", key.clone())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");
        if config.compat.sends_bearer() {
            post = post.header("authorization", format!("Bearer {key}"));
        }
        let response = post.json(body).send().map_err(|e| Rejected {
            status: 0,
            said: format!("the model call failed: {}", scrub(&e.to_string())),
        })?;
        let status = response.status();
        if !status.is_success() {
            let said = response.text().unwrap_or_default();
            return Err(Rejected {
                status: status.as_u16(),
                said: said.trim().chars().take(600).collect::<String>(),
            });
        }
        Ok(response)
    }
}

/// The largest number of times one call re-sends itself in a smaller shape.
/// Two: `strict`, then `cache_control`, then the 400 is the answer.
pub const MAX_DEGRADATIONS: usize = 2;

impl ModelClient for HttpsModelClient {
    /// Count the input — or estimate it, and SAY that is what happened.
    ///
    /// Two ways the count does not happen. The profile may already know there
    /// is none (Ollama has no `/v1/messages/count_tokens`), in which case no
    /// round trip is spent discovering that on every turn; or the endpoint may
    /// answer 404, which is the same discovery made the hard way, remembered so
    /// it is made only once. Either way the budget is still CHECKED — against
    /// an estimate that says it is one.
    fn count_tokens(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelEvent),
    ) -> Result<u64, String> {
        let config = &request.config;
        let estimate = |why: &str, on_event: &mut dyn FnMut(ModelEvent)| -> u64 {
            let counted = estimate_tokens(request);
            on_event(ModelEvent::Note(format!(
                "{why}, so this turn's input budget is an ESTIMATE of about {counted} tokens                  (one per {ESTIMATED_CHARS_PER_TOKEN} characters of request), not a count"
            )));
            counted
        };
        if !config.count_tokens || self.learned(&config.base_url).no_count_tokens {
            return Ok(estimate(
                &format!(
                    "the {} endpoint has no /v1/messages/count_tokens",
                    config.compat.name()
                ),
                on_event,
            ));
        }
        let response = match self.post(config, "/v1/messages/count_tokens", &request.count_body()) {
            Ok(response) => response,
            Err(rejected) if rejected.status == 404 => {
                self.learned(&config.base_url).no_count_tokens = true;
                return Ok(estimate(
                    "this endpoint answered 404 for /v1/messages/count_tokens",
                    on_event,
                ));
            }
            Err(rejected) => {
                return Err(rejected.says());
            }
        };
        let value: serde_json::Value = response
            .json()
            .map_err(|e| format!("the token count did not decode: {e}"))?;
        value
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| format!("the token count carried no `input_tokens`: {value}"))
    }

    /// Stream the reply, dropping what the endpoint will not take.
    ///
    /// A 400 is the one status worth retrying, and only by sending LESS: the
    /// same prompt, the same transcript, the same question, without a feature
    /// the endpoint rejected. Every drop is an event before the retry, so the
    /// reader is told what the answer they are about to read was weakened by,
    /// and the client remembers it for the turns after this one.
    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelEvent),
    ) -> Result<ModelOutcome, String> {
        let config = &request.config;
        let mut shape = self.starting_shape(config);
        for _ in 0..=MAX_DEGRADATIONS {
            let rejected =
                match self.post(config, "/v1/messages", &request.body_shaped(true, shape)) {
                    Ok(response) => {
                        let mut fold = Fold::default();
                        fold_stream(BufReader::new(response), &mut fold, on_event)?;
                        return Ok(fold.outcome);
                    }
                    Err(rejected) => rejected,
                };
            if rejected.status != 400 {
                return Err(rejected.says());
            }
            match degrade(shape, &rejected.said) {
                Some((smaller, note)) => {
                    on_event(ModelEvent::Note(note));
                    shape = smaller;
                    self.remember(config, shape);
                }
                None => {
                    return Err(rejected.says());
                }
            }
        }
        Err(format!(
            "the endpoint answered 400 to every shape this call could take, down to a request              with no `strict` and no `cache_control` ({} base-url {})",
            config.compat.name(),
            config.base_url
        ))
    }
}

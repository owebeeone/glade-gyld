use super::*;

/// Fold a fragment into the notebook and write the result, or answer with the
/// refusal the Gyld host gave (GyldGrythPlugins.md 4.8).
///
/// The sequence, and it runs INSIDE the run for both paths, so a streamed and a
/// synchronous `answer` behave the same:
///
/// 1. the fragment goes to `requests/<run-id>.json`, the host's only operand;
/// 2. `manage_decision_streams.py merge` runs, bounded like any other host;
/// 3. the fragment file is taken away, whatever the host said;
/// 4. on `ok`, the merged module — the host's STDOUT — becomes the planned write's
///    text, and the write and the staging link happen exactly as a whole-module
///    `answer`'s do.
///
/// QUIET on purpose. That stdout is a whole Gyld module, and `gyld.output` is a
/// place a person reads: the lines are not forwarded and one summary line goes out
/// in their place, which is the second half of the answer.
///
/// A refusal is the host's own `code` and `message`.
///
/// `restored` on that refusal answers the question the field asks — IS THE NOTEBOOK
/// AS IT WAS — and every refusal up to and including the host's answer leaves it
/// untouched, so it is. Only a refusal from the write itself says otherwise; a desk
/// reads `false` as "could NOT be put back, check it", which is a thing to say to
/// an owner when it is true and an alarm when it is not (found live, 2026-09-21).
pub(super) fn fold(
    config: &GyldConfig,
    runner: &dyn Runner,
    plan: &Plan,
) -> Result<(Plan, Option<String>), Refusal> {
    let merge = match plan.merge.as_ref() {
        Some(merge) => merge,
        None => {
            return Ok((plan.clone(), None));
        }
    };
    let refused = |code: &str, message: String, untouched: bool| -> Refusal {
        Refusal {
            stream: plan.stream.clone().unwrap_or_default(),
            code: code.to_string(),
            message: verbs::one_line(&message),
            details: None,
            restored: untouched,
        }
    };
    let directory = config.layout.requests();
    if let Err(e) = std::fs::create_dir_all(&directory) {
        return Err(refused(
            outcome::RUN_FAILED,
            format!("cannot create {}: {e}", directory.display()),
            true,
        ));
    }
    if let Err(e) = bundle::replace(&merge.fragment.path, merge.fragment.text.as_bytes()) {
        return Err(refused(
            outcome::RUN_FAILED,
            format!("cannot write {}: {e}", merge.fragment.path.display()),
            true,
        ));
    }
    let host = Plan {
        argv: merge.argv.clone(),
        write: None,
        merge: None,
        output_dir: None,
        ..plan.clone()
    };
    let ran = runner.run(&host, config.limits, &mut |_, _| {});
    // The fragment has served its purpose either way: it is a request document, and
    // a request document left lying about is a request nobody made.
    if let Err(e) = std::fs::remove_file(&merge.fragment.path) {
        if e.kind() != io::ErrorKind::NotFound {
            eprintln!(
                "glade-gyld: could not remove {}: {e}",
                merge.fragment.path.display()
            );
        }
    }
    let out = match ran {
        Ok(out) => out,
        Err(e) => {
            return Err(refused(outcome::RUN_FAILED, e, true));
        }
    };
    let answered = verbs::merge_answer(&out.stdout);
    let said = |key: &str| -> Option<String> {
        answered
            .as_ref()?
            .get(key)?
            .as_str()
            .map(|value| value.to_string())
    };
    let ok = answered
        .as_ref()
        .and_then(|value| value.get("ok"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if !ok {
        // Gyld's own code and message when it wrote one; otherwise the host's last
        // word, which is what every other failed run here is reported by.
        let code = said("code").unwrap_or_else(|| outcome::RUN_FAILED.to_string());
        let message = said("message").unwrap_or_else(|| last_line(&out));
        return Err(refused(&code, message, true));
    }
    let text = match said("text") {
        Some(text) => text,
        None => {
            return Err(refused(
                outcome::RUN_FAILED,
                "the merge host answered ok and printed no merged module".into(),
                true,
            ));
        }
    };
    if text.len() > verbs::MAX_OVERLAY_BYTES {
        return Err(refused(
            outcome::RUN_FAILED,
            format!(
                "the merged module is {} bytes; the limit is {}",
                text.len(),
                verbs::MAX_OVERLAY_BYTES
            ),
            true,
        ));
    }
    let mut written = plan.clone();
    if let Some(write) = written.write.as_mut() {
        write.text = text;
    }
    // The two that may have TOUCHED the notebook: an atomic replace that failed
    // wrote nothing, but a staging link that failed after it did not, and the owner
    // is the one who has to look.
    if let Err(e) = write_overlay(&config.layout, &written) {
        return Err(refused(outcome::RUN_FAILED, e, false));
    }
    if let Err(e) = stage_notebook(&config.layout, &written) {
        return Err(refused(outcome::RUN_FAILED, e, false));
    }
    Ok((written, Some(merged_line(&answered, plan))))
}

/// The one line a merge forwards in place of the module it printed: what was added
/// and which notebook it went into.
fn merged_line(answered: &Option<serde_json::Value>, plan: &Plan) -> String {
    let named = |key: &str| -> Vec<String> {
        answered
            .as_ref()
            .and_then(|value| value.get("added"))
            .and_then(|added| added.get(key))
            .and_then(|list| list.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|item| item.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let added = match named("classes") {
        found if found.is_empty() => named("members"),
        found => found,
    };
    let file = plan
        .overlay
        .as_ref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| plan.stream.clone().unwrap_or_default());
    match added.is_empty() {
        true => format!("merged a fragment into {file}"),
        false => format!("merged {} into {file}", added.join(", ")),
    }
}

/// [`fold`] on a blocking task, so the merge host and the two writes it leads to
/// never run on the async runtime's own thread.
///
/// The plan comes back either way: a refusal needs it to say which verb was
/// refused, and a plan moved onto the task and lost with it could not.
pub(super) async fn folded(
    config: &Arc<GyldConfig>,
    runner: &Arc<dyn Runner>,
    plan: Plan,
) -> Result<(Plan, Option<String>), (Plan, Refusal)> {
    if plan.merge.is_none() {
        return Ok((plan, None));
    }
    let held = plan.clone();
    let work = {
        let config = config.clone();
        let runner = runner.clone();
        tokio::task::spawn_blocking(move || fold(&config, runner.as_ref(), &plan))
    };
    match work.await {
        Ok(Ok(both)) => Ok(both),
        Ok(Err(refusal)) => Err((held, refusal)),
        Err(e) => {
            let refusal = Refusal {
                stream: held.stream.clone().unwrap_or_default(),
                code: outcome::RUN_FAILED.into(),
                message: format!("the merge task failed: {e}"),
                details: None,
                restored: false,
            };
            Err((held, refusal))
        }
    }
}

/// A host's last word: its last non-empty stderr line, else its exit code.
fn last_line(out: &RunOutput) -> String {
    let last = out
        .stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty());
    match last {
        Some(line) => line.to_string(),
        None => format!(
            "the merge host exited {} and said nothing on stderr",
            out.exit
        ),
    }
}

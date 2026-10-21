//! `cmux setup cloud-machine [--dry-run]`: makes sure the person has a cmux
//! Cloud machine for agents, and creates one with their OK.
//!
//! Everything goes through the app: `auth.status` (signed in, and whether
//! Cloud can run in this build), `cloud.machines` (the machines it shows and
//! the creations in flight), and `action.run newCloudMachine`, which from an
//! agent always asks the person first on a card in cmux. The app answers the
//! created machine, or `pending` with a `creation_id` after 30 s (that wait
//! includes the person's answer), or `unavailable` when they declined or
//! there was no window to ask in. A created machine costs money while it
//! runs and does not pause by itself, so the undo kills it.
//!
//! Setup keeps what it asked for in its own state file ([`state`]): the
//! run's idempotency key, written before the create is sent, then the
//! creation or the machine it became. A rerun reads that back in
//! `cloud.machines`, sends a create whose answer it never read again with
//! the same key (the app answers it once), and never starts a second
//! machine while one is on its way. Any machine cmux already shows is
//! enough, and setup leaves it alone. A list cmux has not loaded, or could
//! not, decides nothing. Plan limits are not read beforehand: the app has no
//! read of them on its socket, so a plan that allows no more machines comes
//! back as the create's refusal, with the app's text.

mod state;

use serde_json::{Value, json};

use self::state::Saved;
use super::super::GlobalArgs;
use super::{Outcome, Refusal};

const TASK: &str = "cloud_machine";

/// The command that runs (and reruns) this task.
const RERUN: &str = "cmux setup cloud-machine";

/// The action that creates a machine; from an agent it asks the person.
const CREATE: &str = "newCloudMachine";

/// What a machine costs, said before it is made.
const COST: &str = "A cmux Cloud machine costs money for every hour it runs, at the rate of the \
                    person's plan, and it does not pause by itself";

/// The undo of a create before its machine has an id.
const PLANNED_UNDO: &str = "cmux cloud kill-machine --target machine:MACHINE_ID --confirm";

/// The app's reason when the person declined the card or dismissed it
/// (`CloudStrings.createDeclined`); any other refusal is the app's to say.
const DECLINED_REASON: &str =
    "No Cloud machine was created: the person declined or dismissed it in cmux.";

pub(super) fn run(global: &GlobalArgs, args: &[String]) -> Result<Outcome, Refusal> {
    let dry_run = parse(args)?;
    let mut report = json!({
        "schema_version": 1,
        "task": TASK,
        "dry_run": dry_run,
        "before": Value::Null,
        "after": Value::Null,
        "undo": Value::Null,
        "user_action": Value::Null,
        "machine": Value::Null,
    });
    if !applies(global) {
        report["state"] = json!("not_applicable");
        return Ok(Outcome { report, failed: false });
    }
    let listing = match signed_in(global).and_then(|()| read(global, "cloud.machines")) {
        Ok(listing) => listing,
        Err(action) => return needs_user(report, action),
    };
    let saved = Saved::load();
    let look = look(&listing, &saved);
    if let Some(id) = &look.gone {
        // Killed, or gone from cmux: setup no longer has a machine.
        report["before"] = json!({"id": id, "status": "gone"});
        if !dry_run {
            Saved::default().save()?;
        }
    }
    match look.step {
        Step::Wait(action) => needs_user(report, action),
        Step::Ended(action) => {
            if !dry_run {
                Saved::default().save()?;
            }
            needs_user(report, action)
        }
        Step::Machine { row, made } => {
            let id = row["id"].as_str().unwrap_or_default().to_owned();
            report["after"] = row.clone();
            report["machine"] = row.clone();
            if let Some(note) = paused_note(&row) {
                report["note"] = json!(note);
            }
            let action = machine_step(&row);
            report["state"] = json!(match (&action, made && !dry_run) {
                (Some(_), _) => "needs_user",
                (None, true) => "changed",
                (None, false) => "ok",
            });
            report["user_action"] = action.unwrap_or(Value::Null);
            if made && !dry_run {
                report["undo"] = json!(kill(&id));
                keep(&mut report, &Saved { machine_id: Some(id), ..Saved::default() });
            }
            Ok(Outcome { report, failed: false })
        }
        Step::Ask { key, .. } if dry_run => {
            report["state"] = json!("planned");
            report["undo"] = json!(PLANNED_UNDO);
            report["cost"] = json!(COST);
            if key.is_some() {
                report["note"] = json!(
                    "A create setup sent earlier has no answer yet; the run asks cmux about \
                     that one again and shows no second card"
                );
            }
            Ok(Outcome { report, failed: false })
        }
        Step::Ask { key, known } => create(global, report, key, known),
    }
}

/// The check `cmux setup status` reports, by the same rules as the run: a
/// machine cmux shows (setup's own or another) is `ok`; a creation on its
/// way, or a list cmux has not loaded, is `needs_user` (`wait`); no machine
/// is `needs_setup`. It writes nothing.
pub(super) fn status(global: &GlobalArgs) -> Value {
    if !applies(global) {
        return json!({"task": TASK, "state": "not_applicable"});
    }
    let listing = match signed_in(global).and_then(|()| read(global, "cloud.machines")) {
        Ok(listing) => listing,
        Err(action) => {
            return json!({"task": TASK, "state": "needs_user", "user_action": action, "fix": RERUN});
        }
    };
    match look(&listing, &Saved::load()).step {
        Step::Wait(action) | Step::Ended(action) => {
            json!({"task": TASK, "state": "needs_user", "user_action": action, "fix": RERUN})
        }
        Step::Machine { row, .. } => {
            let action = machine_step(&row);
            let (state, fix) = match action {
                Some(_) => ("needs_user", Some(RERUN)),
                None => ("ok", None),
            };
            json!({"task": TASK, "state": state, "machine": row, "user_action": action,
                   "fix": fix, "note": paused_note(&row)})
        }
        Step::Ask { key: Some(_), .. } => {
            json!({"task": TASK, "state": "needs_user", "user_action": wait_step(), "fix": RERUN})
        }
        Step::Ask { key: None, .. } => json!({"task": TASK, "state": "needs_setup", "fix": RERUN}),
    }
}

/// What the run does next, read from cmux's list and setup's state alone.
struct Look {
    step: Step,
    /// The machine setup kept, when cmux no longer shows it.
    gone: Option<String>,
}

enum Step {
    /// Nothing to decide yet (the list is not loaded, a creation is on its
    /// way); nothing is changed or asked.
    Wait(Value),
    /// Setup's creation ended without a machine: what to tell the person.
    Ended(Value),
    /// A machine cmux shows; `made` when it is the one setup's creation
    /// just became.
    Machine { row: Value, made: bool },
    /// No machine: ask for one, with the key of a create whose answer setup
    /// never read (sent again, so the app answers it once), or a new one.
    Ask { key: Option<String>, known: Vec<String> },
}

fn look(listing: &Value, saved: &Saved) -> Look {
    let wait = |step| Look { step, gone: None };
    if let Some(error) = listing["last_error"].as_str() {
        return wait(Step::Wait(action(
            "wait",
            &format!(
                "cmux could not read the Cloud machines ({error}); run `cmux setup cloud-machine` \
                 again once cmux shows them"
            ),
            Some(RERUN),
        )));
    }
    if listing["loaded"] == false {
        return wait(Step::Wait(action(
            "wait",
            "cmux has not loaded the Cloud machines yet; run `cmux setup cloud-machine` again \
             in a few seconds",
            Some(RERUN),
        )));
    }
    if let Some(creation) = &saved.creation_id {
        match settle(creation, &saved.known, listing) {
            Settled::Machine(id) => {
                let row = machine(listing, &id).map(view).unwrap_or_else(|| {
                    json!({"id": id, "title": null, "status": "provisioning",
                           "target": format!("machine:{id}")})
                });
                return wait(Step::Machine { row, made: true });
            }
            Settled::Pending => return wait(Step::Wait(wait_step())),
            Settled::Ended(action) => return wait(Step::Ended(action)),
        }
    }
    let mut gone = None;
    if let Some(id) = &saved.machine_id {
        if let Some(row) = machine(listing, id) {
            return Look { step: Step::Machine { row: view(row), made: false }, gone: None };
        }
        gone = Some(id.clone());
    }
    let step = if let Some(key) = &saved.key {
        Step::Ask { key: Some(key.clone()), known: saved.known.clone() }
    } else if let Some(row) = live_machines(listing).next() {
        // The person's own machine: there is one, and setup leaves it.
        Step::Machine { row: view(row), made: false }
    } else if creations(listing).any(|row| row["stage"] != "failed") {
        // A machine on its way that setup did not ask for (a click in cmux).
        Step::Wait(action(
            "wait",
            "A Cloud machine is on its way in cmux; run `cmux setup cloud-machine` again once \
             it is up, so setup makes no second one",
            Some(RERUN),
        ))
    } else {
        Step::Ask { key: None, known: machine_ids(listing) }
    };
    Look { step, gone }
}

/// Sends the create, with `key` when setup sent it before and never read
/// its answer. The key is kept before the create goes out, and kept after
/// it when the answer may still come.
fn create(
    global: &GlobalArgs,
    mut report: Value,
    key: Option<String>,
    known: Vec<String>,
) -> Result<Outcome, Refusal> {
    let key = match key {
        Some(key) => {
            eprintln!(
                "cmux: setup cloud-machine: asking cmux again about the machine it asked for"
            );
            key
        }
        None => {
            let key = super::super::command::random_prefixed("mutation")
                .map_err(|error| Refusal::Failed(format!("cmux: setup cloud-machine: {error}")))?;
            Saved { key: Some(key.clone()), known: known.clone(), ..Saved::default() }.save()?;
            eprintln!(
                "cmux: setup cloud-machine: waiting for the person to allow a new Cloud machine \
                 in cmux"
            );
            key
        }
    };
    report["cost"] = json!(COST);
    let params = Value::Object(super::super::app::action_run_params(
        CREATE,
        super::super::app::ActionName::Any,
        super::super::app::action_origin(),
    ));
    let answer = super::super::mcp::agent_app(
        global,
        "action.run",
        params,
        super::super::app::WAITING_RUN_TIMEOUT,
        Some(&key),
    );
    let result = match answer {
        Ok(run) => run["result"].clone(),
        Err(error) => {
            let code = error["code"].as_str().unwrap_or_default();
            if matches!(code, "app.transport" | "timeout") {
                // The create may have gone through: keep its key for the rerun.
                return needs_user(report, wait_step());
            }
            Saved::default().save()?;
            let action = if code == "unavailable" && error["data"]["reason"] == DECLINED_REASON {
                declined()
            } else {
                app_problem(&error)
            };
            return needs_user(report, action);
        }
    };
    if let Some(id) = result["machine_id"].as_str() {
        let row = view(&result);
        report["after"] = row.clone();
        report["machine"] = row;
        report["undo"] = json!(kill(id));
        report["state"] = json!("changed");
        keep(&mut report, &Saved { machine_id: Some(id.to_owned()), ..Saved::default() });
        return Ok(Outcome { report, failed: false });
    }
    // Pending, or an app that answers no machine: the key stays, and with a
    // creation id the rerun follows that creation.
    if let Some(creation) = result["creation_id"].as_str().filter(|id| !id.is_empty()) {
        report["after"] = json!({"creation_id": creation});
        let saved = Saved {
            creation_id: Some(creation.to_owned()),
            key: Some(key),
            known,
            ..Saved::default()
        };
        keep(&mut report, &saved);
    }
    needs_user(report, wait_step())
}

/// Saves `saved` after a change already happened: a failure is a `warning`
/// on the result, never a lost `changed`.
fn keep(report: &mut Value, saved: &Saved) {
    if let Err(Refusal::Failed(message) | Refusal::Usage(message)) = saved.save() {
        report["warning"] = json!(format!(
            "{message}; a rerun of `cmux setup cloud-machine` still finds the machine in cmux"
        ));
    }
}

/// The result when the person must do `action` first.
fn needs_user(mut report: Value, action: Value) -> Result<Outcome, Refusal> {
    report["state"] = json!("needs_user");
    report["user_action"] = action;
    Ok(Outcome { report, failed: false })
}

fn parse(args: &[String]) -> Result<bool, Refusal> {
    let mut dry_run = false;
    for arg in args {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            other => {
                return Err(Refusal::Usage(format!(
                    "cmux: setup cloud-machine: unexpected argument {other:?}"
                )));
            }
        }
    }
    Ok(dry_run)
}

/// Whether there is a cmux app to ask: macOS, or an app socket named by
/// `--app-socket` or `CMUX_SOCKET_PATH`.
fn applies(global: &GlobalArgs) -> bool {
    cfg!(target_os = "macos")
        || global.app_socket.is_some()
        || std::env::var_os("CMUX_SOCKET_PATH").is_some_and(|path| !path.is_empty())
}

/// One app read, or what the person must do because the app did not answer.
fn read(global: &GlobalArgs, method: &str) -> Result<Value, Value> {
    super::super::mcp::agent_app(global, method, json!({}), super::super::app::READ_TIMEOUT, None)
        .map_err(|error| app_problem(&error))
}

/// Ok when the person is signed in to cmux and Cloud can run in this build.
fn signed_in(global: &GlobalArgs) -> Result<(), Value> {
    let auth = read(global, "auth.status")?;
    if let Some(reason) = auth["cloud_unavailable"].as_str() {
        return Err(action("app", &format!("cmux Cloud cannot run here: {reason}"), None));
    }
    if auth["signed_in"] == true {
        return Ok(());
    }
    if auth["restoring"] == true {
        return Err(action(
            "wait",
            "cmux is restoring the person's sign-in; run `cmux setup cloud-machine` again in a \
             few seconds",
            Some(RERUN),
        ));
    }
    Err(action(
        "sign_in",
        "Sign in to cmux: the command opens the sign-in in the browser, where the person \
         finishes it; then run `cmux setup cloud-machine` again",
        Some("cmux cloud sign-in"),
    ))
}

fn declined() -> Value {
    action(
        "declined",
        "No Cloud machine was made: the person declined, or cmux had no window to ask in; ask \
         the person before running `cmux setup cloud-machine` again",
        None,
    )
}

/// What became of a creation setup started.
enum Settled {
    Machine(String),
    Pending,
    /// It ended without a machine: what to tell the person.
    Ended(Value),
}

/// Finds creation `id` in `listing`. A creation leaves the list when the
/// person declined it, or once its machine's workspace shows; then the one
/// machine that was not there before (`known`) is its machine.
fn settle(id: &str, known: &[String], listing: &Value) -> Settled {
    if let Some(row) = creations(listing).find(|row| row["id"] == id) {
        if let Some(machine) = row["machine_id"].as_str() {
            return Settled::Machine(machine.to_owned());
        }
        if row["stage"] == "failed" {
            let detail = row["stage_detail"].as_str().unwrap_or("no reason given");
            return Settled::Ended(action(
                "app",
                &format!("cmux could not make the Cloud machine ({detail}); cmux shows the reason"),
                None,
            ));
        }
        return Settled::Pending;
    }
    let new = machine_ids(listing).into_iter().filter(|id| !known.contains(id)).collect::<Vec<_>>();
    match new.as_slice() {
        [only] => Settled::Machine(only.clone()),
        [] => Settled::Ended(declined()),
        _ => Settled::Ended(action(
            "app",
            &format!(
                "More than one Cloud machine appeared ({}); setup cannot tell which is its own",
                new.join(", ")
            ),
            None,
        )),
    }
}

fn creations(listing: &Value) -> impl Iterator<Item = &Value> {
    listing["creations"].as_array().map(Vec::as_slice).unwrap_or_default().iter()
}

/// The ids of the machines cmux shows that are not destroyed.
fn machine_ids(listing: &Value) -> Vec<String> {
    live_machines(listing).filter_map(|row| row["id"].as_str()).map(str::to_owned).collect()
}

fn live_machines(listing: &Value) -> impl Iterator<Item = &Value> {
    listing["machines"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|row| row["status"] != "destroyed")
}

/// Machine `id` in `listing`, unless it is gone or destroyed.
fn machine<'a>(listing: &'a Value, id: &str) -> Option<&'a Value> {
    live_machines(listing).find(|row| row["id"] == id)
}

/// A machine as setup reports it, from a `cloud.machines` row (`id`) or a
/// create's answer (`machine_id`): `{id, title, status, target}`.
fn view(row: &Value) -> Value {
    let id = row["id"].as_str().or_else(|| row["machine_id"].as_str()).unwrap_or_default();
    json!({"id": id, "title": row["title"], "status": row["status"],
           "target": format!("machine:{id}")})
}

/// What the person must know about a machine that failed, or None.
fn machine_step(row: &Value) -> Option<Value> {
    let id = row["id"].as_str().unwrap_or_default();
    (row["status"] == "failed").then(|| {
        action(
            "app",
            &format!(
                "The Cloud machine {id} failed; cmux shows why. `{}` removes it, and `cmux setup \
                 cloud-machine` then makes a new one",
                kill(id)
            ),
            None,
        )
    })
}

/// How to wake a paused machine; it is still the person's machine.
fn paused_note(row: &Value) -> Option<String> {
    let id = row["id"].as_str().unwrap_or_default();
    (row["status"] == "paused").then(|| {
        format!(
            "The machine is paused: `cmux cloud resume-machine --target machine:{id}` resumes it"
        )
    })
}

fn kill(id: &str) -> String {
    format!("cmux cloud kill-machine --target machine:{id} --confirm")
}

/// The creation is on its way, or the person has not answered yet.
fn wait_step() -> Value {
    action(
        "wait",
        "The Cloud machine is on its way (or the person has not answered cmux yet); run `cmux \
         setup cloud-machine` again in a minute: it makes no second machine",
        Some(RERUN),
    )
}

fn action(kind: &str, text: &str, command: Option<&str>) -> Value {
    json!({"kind": kind, "text": text, "command": command, "url": Value::Null})
}

/// What the person does when the app refused or did not answer.
fn app_problem(error: &Value) -> Value {
    let code = error.get("code").and_then(Value::as_str).unwrap_or_default();
    let message = error.get("message").and_then(Value::as_str).unwrap_or_default();
    let text = match code {
        "app.not_found" | "app.unreachable" => {
            "Open cmux: Cloud machines are made in the cmux app, which is not running; then run \
             `cmux setup cloud-machine` again"
                .to_owned()
        }
        "app.transport" | "timeout" => {
            "cmux did not answer; run `cmux setup cloud-machine` again".to_owned()
        }
        _ => format!("cmux refused ({code}): {message}"),
    };
    action("app", &text, None)
}

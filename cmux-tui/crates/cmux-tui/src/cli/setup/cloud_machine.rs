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
//! runs (auto-pause is off), so the undo kills it.
//!
//! Setup remembers the machine it created (or the creation still on its
//! way) in its own state file, so a rerun checks that machine in
//! `cloud.machines` and never starts a second one while one is pending.
//! A machine of the person's own that already runs is `ok` and not
//! touched. Plan limits are not read beforehand: the app has no read of
//! them on its socket, so a plan that does not allow another machine comes
//! back as the create's refusal, with the app's text.

use std::path::PathBuf;

use serde_json::{Value, json};

use super::super::GlobalArgs;
use super::{Outcome, Refusal};

const TASK: &str = "cloud_machine";

/// The command that runs (and reruns) this task.
const RERUN: &str = "cmux setup cloud-machine";

/// The action that creates a machine; from an agent it asks the person.
const CREATE: &str = "newCloudMachine";

/// What a machine costs, said before it is made.
const COST: &str = "A cmux Cloud machine (4 vCPU, 8 GB) costs about $0.26 an hour while it runs; \
                    it does not pause by itself";

/// The undo of a create before its machine has an id.
const PLANNED_UNDO: &str = "cmux cloud kill-machine --target machine:MACHINE_ID --confirm";

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
    if let Err(action) = signed_in(global) {
        return needs_user(report, action);
    }
    let listing = match read(global, "cloud.machines") {
        Ok(listing) => listing,
        Err(action) => return needs_user(report, action),
    };
    let mut saved = Saved::load();
    let mut resolved = false;
    if let Some(creation) = saved.creation_id.clone() {
        match settle(&creation, &saved.known, &listing) {
            Settled::Machine(id) => {
                saved = Saved { machine_id: Some(id), ..Saved::default() };
                resolved = true;
            }
            Settled::Pending => {
                report["before"] = json!({"creation_id": creation});
                return needs_user(report, wait_step());
            }
            Settled::Ended(action) => {
                if !dry_run {
                    Saved::default().save()?;
                }
                return needs_user(report, action);
            }
        }
    }
    if let Some(id) = saved.machine_id.clone() {
        match machine(&listing, &id) {
            Some(row) => {
                if resolved && !dry_run {
                    saved.save()?;
                }
                report["before"] = row.clone();
                report["after"] = row.clone();
                report["machine"] = row.clone();
                if resolved {
                    report["undo"] = json!(kill(&id));
                }
                let action = machine_step(row);
                report["state"] = json!(match (&action, resolved) {
                    (Some(_), _) => "needs_user",
                    (None, true) => "changed",
                    (None, false) => "ok",
                });
                report["user_action"] = action.unwrap_or(Value::Null);
                return Ok(Outcome { report, failed: false });
            }
            None => {
                // Killed, or gone from cmux: setup makes a new one.
                report["before"] = json!({"machine_id": id, "status": "destroyed"});
                if !dry_run {
                    Saved::default().save()?;
                }
            }
        }
    }
    if let Some(row) = live_machines(&listing).next() {
        // The person's own machine: there is one, and setup leaves it.
        report["after"] = row.clone();
        report["machine"] = row.clone();
        report["state"] = json!("ok");
        return Ok(Outcome { report, failed: false });
    }
    if dry_run {
        report["state"] = json!("planned");
        report["undo"] = json!(PLANNED_UNDO);
        report["cost"] = json!(COST);
        return Ok(Outcome { report, failed: false });
    }
    let known = machine_ids(&listing);
    match create(global) {
        Ok(result) if result["pending"] == true => {
            let creation = result["creation_id"].as_str().unwrap_or_default().to_owned();
            Saved { creation_id: Some(creation.clone()), known, ..Saved::default() }.save()?;
            report["after"] = json!({"creation_id": creation});
            report["cost"] = json!(COST);
            needs_user(report, wait_step())
        }
        Ok(result) => {
            let Some(id) = result["machine_id"].as_str() else {
                return Err(Refusal::Failed(format!(
                    "cmux: setup cloud-machine: the app answered no machine: {result}"
                )));
            };
            Saved { machine_id: Some(id.to_owned()), ..Saved::default() }.save()?;
            report["undo"] = json!(kill(id));
            report["cost"] = json!(COST);
            report["after"] = result.clone();
            report["machine"] = result.clone();
            report["state"] = json!("changed");
            Ok(Outcome { report, failed: false })
        }
        Err(action) => needs_user(report, action),
    }
}

/// The check `cmux setup status` reports: the machine setup made, read in
/// cmux, or `needs_setup` when it made none.
pub(super) fn status(global: &GlobalArgs) -> Value {
    if !applies(global) {
        return json!({"task": TASK, "state": "not_applicable"});
    }
    let saved = Saved::load();
    if saved.machine_id.is_none() && saved.creation_id.is_some() {
        return json!({"task": TASK, "state": "needs_user", "user_action": wait_step(), "fix": RERUN});
    }
    let Some(id) = saved.machine_id else {
        return json!({"task": TASK, "state": "needs_setup", "fix": RERUN});
    };
    match read(global, "cloud.machines") {
        Ok(listing) => match machine(&listing, &id) {
            Some(row) => {
                let action = machine_step(row);
                let (state, fix) = match action {
                    Some(_) => ("needs_user", Some(RERUN)),
                    None => ("ok", None),
                };
                json!({"task": TASK, "state": state, "machine": row, "user_action": action, "fix": fix})
            }
            None => json!({"task": TASK, "state": "needs_setup", "fix": RERUN, "machine_id": id}),
        },
        Err(action) => json!({"task": TASK, "state": "needs_user", "user_action": action}),
    }
}

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

/// Creates a machine: the app asks the person on a card first. The run's
/// `result`, or what the person must do because none was made.
fn create(global: &GlobalArgs) -> Result<Value, Value> {
    eprintln!("cmux: setup cloud-machine: waiting for the person to allow a new Cloud machine");
    let app = super::super::app::action_run_params(
        CREATE,
        super::super::app::ActionName::Any,
        super::super::app::action_origin(),
    );
    match super::super::mcp::agent_app(
        global,
        "action.run",
        Value::Object(app),
        super::super::app::WAITING_RUN_TIMEOUT,
        None,
    ) {
        Ok(run) => Ok(run["result"].clone()),
        Err(error) if error["code"] == "unavailable" && error.pointer("/data/reason").is_some() => {
            Err(declined())
        }
        Err(error) => Err(app_problem(&error)),
    }
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
    let creations = listing["creations"].as_array().map(Vec::as_slice).unwrap_or_default();
    if let Some(row) = creations.iter().find(|row| row["id"] == id) {
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

/// What the person must know about a machine that is not usable, or None.
/// Paused is fine: an agent resumes it.
fn machine_step(row: &Value) -> Option<Value> {
    let id = row["id"].as_str().unwrap_or_default();
    (row["status"] == "failed").then(|| {
        action(
            "app",
            &format!(
                "The Cloud machine {id} reads failed (a paused machine reads failed today too): \
                 `cmux cloud resume-machine --target machine:{id}` resumes a paused one, and \
                 `{}` removes it",
                kill(id)
            ),
            None,
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
        "app.transport" => "cmux did not answer; run `cmux setup cloud-machine` again".to_owned(),
        _ => format!("cmux refused ({code}): {message}"),
    };
    action("app", &text, None)
}

/// The machine setup made, or the creation still on its way, kept between
/// runs; `known` is every machine there was when that creation started.
#[derive(Default)]
struct Saved {
    machine_id: Option<String>,
    creation_id: Option<String>,
    known: Vec<String>,
}

impl Saved {
    /// `<state root>/setup/cloud-machine.json`, beside the daemon's state.
    fn path() -> Option<PathBuf> {
        let sessions = cmux_tui_core::platform::workspace_state_dir()?;
        let root = sessions.parent().unwrap_or(sessions.as_path());
        Some(root.join("setup").join("cloud-machine.json"))
    }

    fn load() -> Self {
        let text = Self::path().and_then(|path| std::fs::read_to_string(path).ok());
        let value = text.and_then(|text| serde_json::from_str::<Value>(&text).ok());
        let Some(value) = value else { return Self::default() };
        let text = |key: &str| value[key].as_str().map(str::to_owned);
        Self {
            machine_id: text("machine_id"),
            creation_id: text("creation_id"),
            known: value["known"]
                .as_array()
                .map(|ids| ids.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default(),
        }
    }

    fn save(&self) -> Result<(), Refusal> {
        let failed = |error: String| {
            Refusal::Failed(format!("cmux: setup cloud-machine: cannot keep its state: {error}"))
        };
        let path = Self::path().ok_or_else(|| failed("no state folder (HOME is unset)".into()))?;
        if self.machine_id.is_none() && self.creation_id.is_none() {
            return match std::fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Err(failed(error.to_string()))
                }
                _ => Ok(()),
            };
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| failed(error.to_string()))?;
        }
        let value = json!({
            "machine_id": self.machine_id,
            "creation_id": self.creation_id,
            "known": self.known,
        });
        std::fs::write(&path, format!("{value}\n")).map_err(|error| failed(error.to_string()))
    }
}

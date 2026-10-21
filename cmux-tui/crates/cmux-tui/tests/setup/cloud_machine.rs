//! `cmux setup cloud-machine` driven the way an agent drives it, against a
//! fake cmux app that shows Cloud machines and asks its person on a card.

use std::collections::HashMap;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::{World, serve_app, shell_words};

/// The app's reason when the person declines the card.
const DECLINED: &str = "No Cloud machine was created: the person declined or dismissed it in cmux.";

/// How the fake app's person answers the New Cloud Machine card.
#[derive(Clone, Copy, PartialEq)]
enum Card {
    Allow,
    Decline,
    /// Allowed, and the machine is not there within the app's 30 s.
    Slow,
    /// The plan allows no more machines.
    Refuse,
    /// Allowed, and the connection drops before the answer: the machine is
    /// made and setup never reads it.
    Drop,
}

/// A fake cmux app that serves what `cmux setup cloud-machine` and `cmux
/// cloud kill-machine` ask: `auth.status`, `cloud.machines` (its
/// `machines`, `creations`, `loaded` and `last_error`), and `action.run` of
/// `newCloudMachine` (the person's card answers as `card`; a run with a key
/// it answered before gets the same answer, as the app does) and of `cloud
/// kill-machine`, whose machine leaves the list. Every request is kept.
struct FakeCloud {
    signed_in: Arc<AtomicBool>,
    loaded: Arc<AtomicBool>,
    last_error: Arc<Mutex<Value>>,
    card: Arc<Mutex<Card>>,
    machines: Arc<Mutex<Vec<Value>>>,
    creations: Arc<Mutex<Vec<Value>>>,
    /// Each card's answer, by the run's idempotency key.
    answers: Arc<Mutex<HashMap<String, Result<Value, Value>>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl FakeCloud {
    fn start(world: &mut World) -> Self {
        let socket = world.home.parent().unwrap().join("app.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        world.app = Some(socket);
        let cloud = Self {
            signed_in: Arc::new(AtomicBool::new(true)),
            loaded: Arc::new(AtomicBool::new(true)),
            last_error: Arc::new(Mutex::new(Value::Null)),
            card: Arc::new(Mutex::new(Card::Allow)),
            machines: Arc::new(Mutex::new(Vec::new())),
            creations: Arc::new(Mutex::new(Vec::new())),
            answers: Arc::new(Mutex::new(HashMap::new())),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let (signed_in, loaded, last_error, card, machines, creations, answers) = (
            cloud.signed_in.clone(),
            cloud.loaded.clone(),
            cloud.last_error.clone(),
            cloud.card.clone(),
            cloud.machines.clone(),
            cloud.creations.clone(),
            cloud.answers.clone(),
        );
        let made = AtomicUsize::new(0);
        serve_app(listener, cloud.requests.clone(), move |method, params| match method {
            "auth.status" => Some(Ok(json!({"signed_in": signed_in.load(Ordering::SeqCst),
                "restoring": false, "cloud_unavailable": null}))),
            "cloud.machines" => {
                let (rows, creations) =
                    (machines.lock().unwrap().clone(), creations.lock().unwrap().clone());
                Some(Ok(json!({"machines": rows, "creations": creations,
                    "loaded": loaded.load(Ordering::SeqCst),
                    "last_error": *last_error.lock().unwrap()})))
            }
            "action.run" if params["action"] == "newCloudMachine" => {
                let key = params["idempotency_key"].as_str().unwrap_or_default().to_owned();
                let mut answers = answers.lock().unwrap();
                if let Some(answer) = answers.get(&key) {
                    return Some(answer.clone());
                }
                let card = *card.lock().unwrap();
                let refusal = |reason: &str| {
                    Err(json!({"code": "unavailable",
                        "message": format!("newCloudMachine unavailable: {reason}"),
                        "data": {"action": "newCloudMachine", "reason": reason}}))
                };
                let answer = match card {
                    Card::Decline => refusal(DECLINED),
                    Card::Refuse => refusal("Your plan allows no more Cloud machines."),
                    Card::Allow | Card::Drop => {
                        let id = format!("vm-{}", made.fetch_add(1, Ordering::SeqCst) + 1);
                        machines
                            .lock()
                            .unwrap()
                            .push(json!({"id": id, "title": "cmux", "status": "running"}));
                        Ok(json!({"action": "newCloudMachine", "ran": true, "result": {
                            "machine_id": id, "title": "cmux", "status": "provisioning",
                            "target": format!("machine:{id}")}}))
                    }
                    Card::Slow => {
                        let id = format!("cr-{}", made.fetch_add(1, Ordering::SeqCst) + 1);
                        creations
                            .lock()
                            .unwrap()
                            .push(json!({"id": id, "machine_id": null, "stage": "creating"}));
                        Ok(json!({"action": "newCloudMachine", "ran": true, "result": {
                            "pending": true, "status": "creating", "creation_id": id}}))
                    }
                };
                answers.insert(key, answer.clone());
                (card != Card::Drop).then_some(answer)
            }
            "action.run" if params["action"] == "cloud kill-machine" && params["cli"] == true => {
                if params["args"]["confirm"] != true {
                    return Some(Err(
                        json!({"code": "confirmation_required", "message": "pass --confirm"}),
                    ));
                }
                let id =
                    params["target"].as_str().unwrap_or_default().trim_start_matches("machine:");
                let mut machines = machines.lock().unwrap();
                let before = machines.len();
                // The app lists no destroyed machine.
                machines.retain(|row| row["id"] != id);
                Some(if machines.len() < before {
                    Ok(json!({"action": "cloud kill-machine", "ran": true, "result": {}}))
                } else {
                    Err(json!({"code": "not_found", "message": "no such machine"}))
                })
            }
            _ => Some(Err(json!({"code": "method_not_found", "message": "unknown"}))),
        });
        cloud
    }

    /// How many cards the person was shown (a run sent again with its key
    /// shows none).
    fn cards(&self) -> usize {
        self.answers.lock().unwrap().len()
    }

    /// The idempotency key of every `newCloudMachine` run, in order.
    fn create_keys(&self) -> Vec<Value> {
        let requests = self.requests.lock().unwrap();
        requests
            .iter()
            .filter(|r| r["params"]["action"] == "newCloudMachine")
            .map(|r| r["params"]["idempotency_key"].clone())
            .collect()
    }
}

/// A Cloud machine driven the way an agent drives it: signed out is the
/// person's sign-in, a dry run asks nothing, the person declines and then
/// allows the machine on cmux's card, a rerun makes no second one, the
/// undo kills it, and a run after that makes a new one.
#[test]
fn cmux_setup_cloud_machine_makes_one_with_the_persons_ok_and_never_a_second() {
    let mut world = World::new("cloud");
    let app = FakeCloud::start(&mut world);

    let status = world.json(&["setup", "status"]);
    let check = &status["checks"][5];
    assert_eq!(check["task"], "cloud_machine", "{status}");
    assert_eq!(check["state"], "needs_setup", "{check}");
    let fix = check["fix"].as_str().expect("status names its fix").to_owned();
    assert_eq!(fix, "cmux setup cloud-machine");

    app.signed_in.store(false, Ordering::SeqCst);
    let signed_out = world.follow(&fix);
    assert_eq!(signed_out["state"], "needs_user", "{signed_out}");
    assert_eq!(signed_out["user_action"]["kind"], "sign_in", "{signed_out}");
    assert_eq!(signed_out["user_action"]["command"], "cmux cloud sign-in");
    app.signed_in.store(true, Ordering::SeqCst);

    let planned = world.json(&["setup", "cloud-machine", "--dry-run"]);
    assert_eq!(planned["state"], "planned", "{planned}");
    assert!(planned["cost"].is_string(), "{planned}");
    assert!(app.create_keys().is_empty(), "a dry run asked the person");

    // The person declines: nothing made, and the agent asks before trying again.
    *app.card.lock().unwrap() = Card::Decline;
    let declined = world.follow(&fix);
    assert_eq!(declined["state"], "needs_user", "{declined}");
    assert_eq!(declined["user_action"]["kind"], "declined", "{declined}");
    assert_eq!(declined["undo"], Value::Null);

    // The person allows it: one machine, and its undo.
    *app.card.lock().unwrap() = Card::Allow;
    let made = world.follow(&fix);
    assert_eq!(made["state"], "changed", "{made}");
    let machine = json!({"id": "vm-1", "title": "cmux", "status": "provisioning",
        "target": "machine:vm-1"});
    assert_eq!(made["machine"], machine, "{made}");
    let undo = made["undo"].as_str().expect("a change names its undo").to_owned();
    assert_eq!(undo, "cmux cloud kill-machine --target machine:vm-1 --confirm");
    assert_eq!(app.cards(), 2);

    // A rerun reads the machine it made, in the same shape, and asks nothing.
    let again = world.follow(&fix);
    assert_eq!(again["state"], "ok", "{again}");
    let running = json!({"id": "vm-1", "title": "cmux", "status": "running",
        "target": "machine:vm-1"});
    assert_eq!(again["machine"], running, "{again}");
    assert_eq!(app.cards(), 2, "a rerun made a second machine");
    let status = world.json(&["setup", "status"]);
    let check = &status["checks"][5];
    assert_eq!(
        (&check["state"], &check["machine"]["id"]),
        (&json!("ok"), &json!("vm-1")),
        "{check}"
    );

    // The undo, as the agent's shell runs it, kills that machine.
    let words = shell_words(&undo);
    let args = words[1..].iter().map(String::as_str).collect::<Vec<_>>();
    let output = world.run(&args);
    assert!(output.status.success(), "{undo}: {}", String::from_utf8_lossy(&output.stderr));
    assert!(app.machines.lock().unwrap().is_empty());

    // Its machine is gone, so setup makes a new one.
    assert_eq!(world.json(&["setup", "status"])["checks"][5]["state"], "needs_setup");
    let remade = world.follow(&fix);
    assert_eq!(remade["state"], "changed", "{remade}");
    assert_eq!(remade["before"], json!({"id": "vm-1", "status": "gone"}), "{remade}");
    assert_eq!(remade["machine"]["id"], "vm-2", "{remade}");
    assert_eq!(app.cards(), 3);

    let output = world.run(&["setup", "cloud-machine", "now"]);
    assert_eq!(output.status.code(), Some(2));
}

/// A machine that takes longer than the app's wait: setup keeps the
/// creation, reruns wait for it without asking again, a dry run reads it
/// without claiming it, and the machine it becomes is setup's own.
#[test]
fn cmux_setup_cloud_machine_waits_for_a_slow_creation_instead_of_a_second() {
    let mut world = World::new("cloudslow");
    let app = FakeCloud::start(&mut world);
    *app.card.lock().unwrap() = Card::Slow;
    let pending = world.json(&["setup", "cloud-machine"]);
    assert_eq!(pending["state"], "needs_user", "{pending}");
    assert_eq!(pending["user_action"]["kind"], "wait", "{pending}");
    assert_eq!(pending["after"]["creation_id"], "cr-1", "{pending}");
    let rerun = pending["user_action"]["command"].as_str().unwrap().to_owned();

    let still = world.follow(&rerun);
    assert_eq!(still["user_action"]["kind"], "wait", "{still}");
    assert_eq!(world.json(&["setup", "status"])["checks"][5]["user_action"]["kind"], "wait");
    assert_eq!(app.cards(), 1, "a rerun asked for a second machine");

    // The machine comes up; the creation names it.
    app.machines.lock().unwrap().push(json!({"id": "vm-9", "title": "cmux", "status": "running"}));
    app.creations.lock().unwrap()[0]["machine_id"] = json!("vm-9");
    let looked = world.json(&["setup", "cloud-machine", "--dry-run"]);
    assert_eq!(looked["state"], "ok", "a dry run claimed a change: {looked}");
    assert_eq!((&looked["before"], &looked["undo"]), (&Value::Null, &Value::Null), "{looked}");
    let made = world.follow(&rerun);
    assert_eq!(made["state"], "changed", "{made}");
    assert_eq!(made["machine"]["id"], "vm-9", "{made}");
    assert_eq!(made["undo"], "cmux cloud kill-machine --target machine:vm-9 --confirm");
    assert_eq!(world.follow(&rerun)["state"], "ok");
    assert_eq!(app.cards(), 1);

    // A creation that left the list once its workspace showed: the new machine is its.
    let mut world = World::new("cloudgone");
    let app = FakeCloud::start(&mut world);
    *app.card.lock().unwrap() = Card::Slow;
    assert_eq!(world.json(&["setup", "cloud-machine"])["user_action"]["kind"], "wait");
    app.creations.lock().unwrap().clear();
    app.machines.lock().unwrap().push(json!({"id": "vm-5", "title": "cmux", "status": "running"}));
    let made = world.json(&["setup", "cloud-machine"]);
    assert_eq!(made["state"], "changed", "{made}");
    assert_eq!(made["machine"]["id"], "vm-5", "{made}");
    assert_eq!(app.cards(), 1);
}

/// A list cmux has not loaded, or could not read, decides nothing: the
/// machine setup made is kept and nothing is asked. A machine on its way
/// that setup did not ask for is waited for too.
#[test]
fn cmux_setup_cloud_machine_decides_nothing_off_a_list_cmux_has_not_loaded() {
    let mut world = World::new("cloudload");
    let app = FakeCloud::start(&mut world);
    assert_eq!(world.json(&["setup", "cloud-machine"])["state"], "changed");

    // Before cmux's first refresh the list is empty.
    let rows = std::mem::take(&mut *app.machines.lock().unwrap());
    app.loaded.store(false, Ordering::SeqCst);
    let early = world.json(&["setup", "cloud-machine"]);
    assert_eq!(early["state"], "needs_user", "{early}");
    assert_eq!(early["user_action"]["kind"], "wait", "{early}");
    assert_eq!(world.json(&["setup", "status"])["checks"][5]["user_action"]["kind"], "wait");

    // A refresh that failed leaves the list as it was, and says so.
    app.loaded.store(true, Ordering::SeqCst);
    *app.last_error.lock().unwrap() = json!("The network connection was lost.");
    let failed = world.json(&["setup", "cloud-machine"]);
    assert_eq!(failed["user_action"]["kind"], "wait", "{failed}");
    assert_eq!(app.cards(), 1, "setup made a machine off a list it could not trust");

    // Loaded again: setup's machine is still its own.
    *app.last_error.lock().unwrap() = Value::Null;
    *app.machines.lock().unwrap() = rows;
    let back = world.json(&["setup", "cloud-machine"]);
    assert_eq!((&back["state"], &back["machine"]["id"]), (&json!("ok"), &json!("vm-1")), "{back}");

    // A machine the person started in cmux is on its way: no second one.
    let mut world = World::new("cloudtheirs");
    let app = FakeCloud::start(&mut world);
    app.creations.lock().unwrap().push(json!({"id": "c", "machine_id": null, "stage": "booting"}));
    let theirs = world.json(&["setup", "cloud-machine"]);
    assert_eq!(theirs["user_action"]["kind"], "wait", "{theirs}");
    assert_eq!(world.json(&["setup", "status"])["checks"][5]["user_action"]["kind"], "wait");
    assert_eq!(app.cards(), 0);
}

/// A create whose answer never came back: the key was kept before it was
/// sent, so the rerun sends it again and the app answers the one machine it
/// made, with no second card.
#[test]
fn cmux_setup_cloud_machine_sends_an_unanswered_create_again_with_its_key() {
    let mut world = World::new("clouddrop");
    let app = FakeCloud::start(&mut world);
    *app.card.lock().unwrap() = Card::Drop;
    let lost = world.json(&["setup", "cloud-machine"]);
    assert_eq!(lost["state"], "needs_user", "{lost}");
    assert_eq!(lost["user_action"]["kind"], "wait", "{lost}");
    assert_eq!(world.json(&["setup", "status"])["checks"][5]["user_action"]["kind"], "wait");

    *app.card.lock().unwrap() = Card::Allow;
    let made = world.json(&["setup", "cloud-machine"]);
    assert_eq!(made["state"], "changed", "{made}");
    assert_eq!(made["machine"]["id"], "vm-1", "{made}");
    let keys = app.create_keys();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert!(keys[0].is_string() && keys[0] == keys[1], "a new key: {keys:?}");
    assert_eq!(app.cards(), 1, "the person saw a second card");
    assert_eq!(world.json(&["setup", "cloud-machine"])["state"], "ok");
}

/// A refusal that is not the person's decline (a plan with no room) is the
/// app's to say, and the next run asks afresh.
#[test]
fn cmux_setup_cloud_machine_names_a_plan_refusal_as_the_apps() {
    let mut world = World::new("cloudplan");
    let app = FakeCloud::start(&mut world);
    *app.card.lock().unwrap() = Card::Refuse;
    let refused = world.json(&["setup", "cloud-machine"]);
    assert_eq!(refused["state"], "needs_user", "{refused}");
    let action = &refused["user_action"];
    assert_eq!(action["kind"], "app", "{refused}");
    let text = action["text"].as_str().unwrap();
    assert!(text.contains("Your plan allows no more Cloud machines."), "{text}");

    *app.card.lock().unwrap() = Card::Allow;
    assert_eq!(world.json(&["setup", "cloud-machine"])["state"], "changed");
    let keys = app.create_keys();
    assert_ne!(keys[0], keys[1], "a refused create was sent again: {keys:?}");
}

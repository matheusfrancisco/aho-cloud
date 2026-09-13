//! End to end over synthetic session logs: what each derivation drafts, at which strength, from
//! which entries, and what it refuses to copy or trust.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure

use aho_log::{Entry, EntryKind, LogStore, MemoryLog, OpKind, OpStatus, ToolOutcome};
use aho_net::{NetworkConfig, PortMap, Transport};
use aho_types::{AgentId, CallId, OpId, Principal, TurnId};
use cloud_contract::draft::{Draft, Strength};
use cloud_draft::{Error, SessionSummary, draft_from_entries, draft_from_jsonl, verify_jsonl};
use serde_json::{Value, json};

const SID: &str = "1a2b3c";

/// How a tool call ended.
#[derive(Clone, Copy)]
enum Fate {
    /// Op completed; `exit` is the shell's exit status in the result value.
    Exit(i32),
    /// Killed by the time limit (a foreground server).
    TimedOut,
    /// No `OpDone` yet when the log ends.
    Running,
    /// The tool itself failed.
    Failed,
}

struct Session {
    log: MemoryLog,
    calls: u64,
}

fn agent() -> Principal {
    Principal::Agent { id: AgentId(7), turn: TurnId(1) }
}

impl Session {
    fn new() -> Self {
        Session { log: MemoryLog::new(), calls: 0 }
    }

    fn append(&self, by: Principal, kind: EntryKind) -> Entry {
        self.log.append(None, by, kind).unwrap()
    }

    /// One tool call with its op, an optional child receipt (by `receipt_by`), result and op end.
    /// Returns the `ToolCall` entry.
    fn call(&mut self, tool: &str, args: Value, fate: Fate, receipt_by: Option<&str>) -> Entry {
        self.calls += 1;
        let call = CallId(format!("toolu_{}", self.calls));
        let op = OpId(self.calls);
        let e = self.append(
            agent(),
            EntryKind::ToolCall { call: call.clone(), tool: tool.into(), args: args.clone(), class: aho_types::Consequence::SpawnsProcess },
        );
        self.append(
            Principal::system("ops"),
            EntryKind::OpStarted { op, call: call.clone(), kind: OpKind::Process, rerunnable: false, external_effect: true },
        );
        if let Some(by) = receipt_by {
            let pid = 4000 + self.calls as u32;
            self.append(
                Principal::system(by),
                EntryKind::Custom {
                    ns: "Child".into(),
                    name: "Spawned".into(),
                    body: json!({"pid": pid, "start_time": 1, "kind": "job", "label": tool}),
                },
            );
        }
        let (outcome, value, status) = match fate {
            Fate::Exit(code) => (
                ToolOutcome::Ok,
                json!({"command": args.get("command"), "exit": code, "timed_out": false}),
                Some(OpStatus::Completed { code: 0 }),
            ),
            Fate::TimedOut => (
                ToolOutcome::Ok,
                json!({"command": args.get("command"), "exit": null, "timed_out": true}),
                Some(OpStatus::Completed { code: 0 }),
            ),
            Fate::Running => (ToolOutcome::Ok, Value::Null, None),
            Fate::Failed => {
                (ToolOutcome::Err { code: "FAILED".into(), reason: "no".into() }, Value::Null, Some(OpStatus::Failed { err: "no".into() }))
            }
        };
        if !matches!(fate, Fate::Running) {
            self.append(
                Principal::system("tool"),
                EntryKind::ToolResult { call, outcome, value, content: vec![], spill_ref: None, meta: None, late: false },
            );
        }
        if let Some(status) = status {
            self.append(Principal::system("ops"), EntryKind::OpDone { op, status });
        }
        e
    }

    fn bash(&mut self, command: &str, fate: Fate) -> Entry {
        self.call("bash", json!({"command": command}), fate, Some("ops"))
    }

    fn write(&mut self, path: &str, content: &str) -> Entry {
        self.call("write", json!({"path": path, "content": content}), Fate::Exit(0), None)
    }

    fn net_configured(&self, by: Principal, ports: &[u16]) -> Entry {
        let mut cfg = NetworkConfig::default();
        cfg.port_maps = ports
            .iter()
            .map(|&p| PortMap {
                protocol: Transport::Tcp,
                host: format!("127.0.0.1:{}", p + 10000).parse().unwrap(),
                guest: "g".into(),
                guest_port: p,
            })
            .collect();
        self.append(
            by,
            EntryKind::Custom { ns: "Net".into(), name: "Configured".into(), body: json!({"change": "aho net", "config": cfg}) },
        )
    }

    fn flow(&self, by: &str, destination: &str, allowed: bool) -> Entry {
        let body = json!({
            "network": "n1", "guest": "g", "protocol": "tcp", "source": "10.85.1.2:40000", "destination": destination,
            "allowed": allowed, "reason": "policy", "unlock": null, "unix_ms": 1
        });
        self.append(Principal::system(by), EntryKind::Custom { ns: "Net".into(), name: "Flow".into(), body })
    }

    fn entries(&self) -> Vec<Entry> {
        self.log.read_branch(self.log.head().unwrap()).unwrap()
    }

    fn draft(&self) -> Draft {
        draft_from_entries(SID, &self.entries())
    }

    fn jsonl(&self) -> String {
        self.entries().iter().map(|e| serde_json::to_string(e).unwrap() + "\n").collect()
    }
}

fn uri(e: &Entry) -> String {
    format!("aho-log:{SID}#{}", e.hash.to_hex())
}

fn field<'d>(d: &'d Draft, pointer: &str) -> &'d cloud_contract::draft::DraftField {
    d.fields.get(pointer).unwrap_or_else(|| panic!("no field {pointer} in {:#?}", d.fields.keys().collect::<Vec<_>>()))
}

#[test]
fn build_is_the_last_bash_build_whose_op_completed_with_code_zero() {
    let mut s = Session::new();
    s.bash("npm ci && npm run build", Fate::Exit(0));
    let docker = s.bash("docker build -f deploy/Dockerfile.prod -t app .", Fate::Exit(0));
    s.bash("docker build ./broken", Fate::Exit(1));
    s.bash("cargo build --release", Fate::Failed);
    s.bash("npm run lint", Fate::Exit(0));
    s.call("run_code", json!({"command": "docker build ./other"}), Fate::Exit(0), None);
    let d = s.draft();

    assert_eq!(field(&d, "/build/kind").value, json!("dockerfile"));
    assert_eq!(field(&d, "/build/path").value, json!("deploy/Dockerfile.prod"));
    assert_eq!(field(&d, "/build/context").value, json!("."));
    for p in ["/build/kind", "/build/path", "/build/context"] {
        assert_eq!(field(&d, p).strength, Strength::Chained, "{p}");
        assert_eq!(field(&d, p).evidence[0].uri, uri(&docker), "{p}");
    }
    assert!(field(&d, "/build/kind").evidence[0].note.contains("op done aho-log:"), "the note points at the OpDone entry too");
    assert!(!d.unconfirmed().iter().any(|p| p.starts_with("/build")), "chained build fields need no confirmation");
}

#[test]
fn command_builds_chain_the_argv_and_infer_the_base_image() {
    let mut s = Session::new();
    let go = s.bash("cd api && go build -o bin/api ./cmd/api", Fate::Exit(0));
    let d = s.draft();
    assert_eq!(field(&d, "/build/kind").value, json!("command"));
    assert_eq!(field(&d, "/build/command").value, json!(["go", "build", "-o", "bin/api", "./cmd/api"]));
    assert_eq!(field(&d, "/build/command").strength, Strength::Chained);
    assert_eq!(field(&d, "/build/command").evidence[0].uri, uri(&go));
    assert_eq!(field(&d, "/build/base_image").value, json!("golang:1"));
    assert_eq!(field(&d, "/build/base_image").strength, Strength::Inferred);
    assert_eq!(d.unconfirmed(), vec!["/build/base_image".to_string()]);

    let mut none = Session::new();
    none.bash("docker build .", Fate::Running);
    none.bash("go build ./...", Fate::TimedOut);
    assert!(none.draft().fields.is_empty(), "a build that did not finish with code 0 is not evidence of anything");
}

#[test]
fn processes_are_chained_only_through_receipts_from_system_ops() {
    let mut s = Session::new();
    s.bash("npm run build", Fate::Exit(0));
    let web = s.bash("PORT=4000 nohup node dist/server.js > server.log 2>&1 &", Fate::Exit(0));
    let forged = s.call("bash", json!({"command": "python worker.py"}), Fate::TimedOut, Some("bash"));
    s.bash("node scripts/migrate.js", Fate::Exit(0));
    s.bash("npm start", Fate::Exit(1));
    let d = s.draft();

    assert_eq!(field(&d, "/processes/0/command").value, json!(["node", "dist/server.js"]));
    assert_eq!(field(&d, "/processes/0/command").strength, Strength::Chained);
    assert_eq!(field(&d, "/processes/0/command").evidence[0].uri, uri(&web));
    assert!(field(&d, "/processes/0/command").evidence[0].note.contains("receipt aho-log:"));
    assert_eq!(field(&d, "/processes/0/port").value, json!(4000));
    assert_eq!(field(&d, "/processes/0/port").strength, Strength::Inferred);
    assert_eq!(field(&d, "/processes/0/name").value, json!("web"));

    assert_eq!(field(&d, "/processes/1/command").value, json!(["python", "worker.py"]));
    assert_eq!(field(&d, "/processes/1/command").strength, Strength::Inferred, "a receipt a tool appended names no child");
    assert_eq!(field(&d, "/processes/1/command").evidence[0].uri, uri(&forged));
    assert_eq!(field(&d, "/processes/1/name").value, json!("worker"));

    assert!(!d.fields.contains_key("/processes/2/command"), "finished scripts and foreground runs that exited are not processes");
}

#[test]
fn operator_port_maps_chain_the_port_and_forged_configs_are_ignored() {
    let mut s = Session::new();
    s.bash("uvicorn main:app --host 0.0.0.0 --port 8000", Fate::TimedOut);
    s.bash("gunicorn app:app &", Fate::Exit(0));
    s.net_configured(Principal::Operator, &[3000]);
    let cfg = s.net_configured(Principal::Operator, &[8000, 9000]);
    s.net_configured(Principal::system("bash"), &[6666]);
    let d = s.draft();

    assert_eq!(field(&d, "/processes/0/command").value, json!(["uvicorn", "main:app", "--host", "0.0.0.0", "--port", "8000"]));
    let port = field(&d, "/processes/0/port");
    assert_eq!((port.value.clone(), port.strength), (json!(8000), Strength::Chained));
    assert_eq!(port.evidence[0].uri, uri(&cfg), "the latest operator config wins");
    assert_eq!(field(&d, "/processes/0/expose").value, json!("public"));
    assert_eq!(field(&d, "/processes/0/health/kind").value, json!("http"));

    // 9000 has no server naming it: it goes to the server that names no port.
    assert_eq!(field(&d, "/processes/1/command").value, json!(["gunicorn", "app:app"]));
    assert_eq!(field(&d, "/processes/1/port").value, json!(9000));
    assert_eq!(field(&d, "/processes/1/port").strength, Strength::Chained);
    assert_eq!(field(&d, "/processes/1/name").value, json!("web-2"));
    let ports: Vec<Value> = (0..).map_while(|i| d.fields.get(&format!("/processes/{i}/port"))).map(|f| f.value.clone()).collect();
    assert_eq!(ports, vec![json!(8000), json!(9000)], "only the latest operator config counts; a Net/Configured from a tool is ignored");
}

#[test]
fn egress_comes_from_urls_minus_dev_hosts_and_flows_are_observed_ips() {
    let mut s = Session::new();
    s.bash("curl -fsSL https://registry.npmjs.org/express && git clone https://github.com/acme/app", Fate::Exit(0));
    let code = s.write(
        "src/pay.ts",
        r#"const r = await fetch("https://api.stripe.com/v1/charges");
const ai = new OpenAI({ baseURL: "https://api.openai.com/v1" });
const svg = '<svg xmlns="http://www.w3.org/2000/svg">'; fetch('http://localhost:3000/x');"#,
    );
    let doc = s.call("web_fetch", json!({"url": "https://hooks.acme.io/in"}), Fate::Exit(0), None);
    s.call("write", json!({"path": "src/ghost.ts", "content": "fetch('https://ghost.acme.io')"}), Fate::Failed, None);
    let again = s.write("src/refund.ts", "fetch('https://api.stripe.com/v1/refunds')");
    let allowed = s.flow("net-userspace", "93.184.216.34:443", true);
    s.flow("net-userspace", "93.184.216.34:443", true);
    s.flow("net-userspace", "203.0.113.9:443", false);
    s.flow("net-userspace", "10.85.1.1:53", true);
    s.flow("net-userspace", "192.168.1.20:5432", true);
    s.flow("bash", "198.51.100.7:443", true);
    let d = s.draft();

    let egress: Vec<(Value, Strength)> =
        (0..).map_while(|i| d.fields.get(&format!("/egress/{i}"))).map(|f| (f.value.clone(), f.strength)).collect();
    assert_eq!(
        egress,
        vec![
            (json!("api.stripe.com"), Strength::Inferred),
            (json!("hooks.acme.io"), Strength::Inferred),
            (json!("93.184.216.34"), Strength::Observed)
        ]
    );
    let stripe = field(&d, "/egress/0");
    assert_eq!(stripe.evidence.iter().map(|e| e.uri.clone()).collect::<Vec<_>>(), vec![uri(&code), uri(&again)]);
    assert!(stripe.evidence[0].note.contains("src/pay.ts"));
    assert_eq!(field(&d, "/egress/1").evidence[0].uri, uri(&doc));
    let flow = field(&d, "/egress/2");
    assert_eq!(flow.evidence.len(), 2);
    assert_eq!(flow.evidence[0].uri, format!("net:{SID}#{}", allowed.hash.to_hex()));
    assert!(
        flow.evidence[0].note.contains("93.184.216.34")
            && !flow.evidence[0].note.contains(":443")
            && !flow.evidence[0].note.contains("10.85")
    );
    assert_eq!(d.unconfirmed().iter().filter(|p| p.starts_with("/egress")).count(), 3, "inferred and observed egress needs a human");
}

#[test]
fn secret_names_come_from_env_reads_in_code_that_was_written() {
    let mut s = Session::new();
    let server = s.write(
        "server.ts",
        "const port = process.env.PORT ?? 3000;\nconst stripe = new Stripe(process.env.STRIPE_SECRET_KEY);\nconst db = process.env[\"DATABASE_URL\"];",
    );
    s.call("edit", json!({"path": "app.py", "old_string": "x", "new_string": "token = os.getenv('SLACK_BOT_TOKEN')"}), Fate::Exit(0), None);
    s.call(
        "str_replace_editor",
        json!({"command": "create", "path": "main.rs", "file_text": "let k = std::env::var(\"STRIPE_SECRET_KEY\");"}),
        Fate::Exit(0),
        None,
    );
    s.call("write", json!({"path": "lost.rb", "content": "ENV['NEVER_WRITTEN']"}), Fate::Failed, None);
    s.bash("echo $SHELL_ONLY && printenv OTHER", Fate::Exit(0));
    let d = s.draft();

    let names: Vec<Value> = (0..).map_while(|i| d.fields.get(&format!("/secrets/{i}/name"))).map(|f| f.value.clone()).collect();
    assert_eq!(names, vec![json!("STRIPE_SECRET_KEY"), json!("DATABASE_URL"), json!("SLACK_BOT_TOKEN")]);
    let stripe = field(&d, "/secrets/0/name");
    assert_eq!(stripe.strength, Strength::Inferred);
    assert_eq!(stripe.evidence.len(), 2, "both reads are evidence");
    assert_eq!(stripe.evidence[0].uri, uri(&server));
    assert!(stripe.evidence[0].note.contains("process.env.STRIPE_SECRET_KEY") && stripe.evidence[0].note.contains("line 2"));
}

#[test]
fn values_that_look_like_secrets_are_never_copied() {
    let secrets = [
        "ghp_Zx8QmR2vT7wLp4sK9dF3hJ6nB1cV5yA0",
        "npm_Q7wE3rT9yU1iO5pA2sD8fG4hJ6kL0zXc",
        "sk_live_51HqLyjWDarjtT1zdp7dcXQzZ",
        "hunter2hunter2",
        "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv",
    ];
    let mut s = Session::new();
    s.bash(&format!("API_TOKEN={} node server.js &", secrets[0]), Fate::Exit(0));
    s.bash(&format!("docker build --build-arg NPM_TOKEN={} -t app .", secrets[1]), Fate::Exit(0));
    s.write("src/keys.ts", &format!("const key = \"{}\";\nconst db = \"postgres://admin:{}@db.acme.io/prod\";", secrets[2], secrets[3]));
    s.call("bash", json!({"command": format!("node worker.js --api-key {} &", secrets[4])}), Fate::Exit(0), Some("ops"));
    let d = s.draft();

    let text = serde_json::to_string(&d).unwrap();
    for secret in secrets {
        assert!(!text.contains(secret), "{secret} leaked into the draft: {text}");
    }
    assert_eq!(field(&d, "/processes/0/command").value, json!(["node", "server.js"]), "assignments are dropped, the command kept");
    assert!(field(&d, "/build/kind").evidence[0].note.contains("NPM_TOKEN=[redacted]"));
    assert!(!d.fields.contains_key("/processes/1/command"), "a command carrying a credential is not drafted at all");
    assert!(d.fields.contains_key("/processes/1/name"), "the process itself still is");
    assert!(!text.contains("postgres://"), "only hosts are copied from URLs");
}

#[test]
fn a_draft_completed_by_a_human_is_a_valid_manifest() {
    let mut s = Session::new();
    s.bash("docker build .", Fate::Exit(0));
    s.bash("node server.js &", Fate::Exit(0));
    s.net_configured(Principal::Operator, &[8080]);
    s.write("server.js", "fetch('https://api.stripe.com/v1/charges', { headers: { authorization: process.env.STRIPE_KEY } })");
    let mut d = s.draft();
    for p in d.unconfirmed() {
        assert!(d.confirm(&p));
    }
    d.declare("/api_version", json!(cloud_contract::manifest::API_VERSION));
    d.declare("/project", json!("shop"));
    d.declare("/source", json!({"kind": "archive", "hash": format!("blake3:{}", "a".repeat(64))}));
    d.declare("/secrets/0/reach", json!(["api.stripe.com"]));
    d.declare("/auth", json!({"mode": "none"}));
    d.declare("/data_class", json!("none"));
    d.declare("/owner", json!({"name": "Ana", "email": "ana@shop.io"}));
    d.declare("/budget", json!({"ceiling_usd_month": 20, "degrade": "sleep"}));
    let m = d.into_manifest().unwrap();
    assert_eq!(cloud_contract::validate(&m), Ok(()));
    assert_eq!(m.processes[0].port, Some(8080));
    assert_eq!(m.processes[0].command, Some(vec!["node".to_string(), "server.js".to_string()]));
    assert_eq!(m.egress, vec!["api.stripe.com"]);
}

#[test]
fn jsonl_drafts_verify_the_chain_and_reject_tampering() {
    let mut s = Session::new();
    s.bash("npm run build", Fate::Exit(0));
    s.bash("npm start", Fate::TimedOut);
    let text = s.jsonl();
    let entries = s.entries();

    let (d, summary) = draft_from_jsonl(&text).unwrap();
    let root = entries[0].hash.to_hex();
    assert_eq!(
        summary,
        SessionSummary { session_id: root.clone(), head_hash: entries.last().unwrap().hash.to_hex(), entries: entries.len() as u64 }
    );
    assert_eq!(d, draft_from_entries(&root, &entries), "same draft as from the entries, keyed by the root hash");
    assert_eq!(verify_jsonl(&text).unwrap(), entries);

    // an edited argument no longer hashes to the recorded hash
    let edited = text.replacen("npm run build", "npm run buildx", 1);
    assert_eq!(draft_from_jsonl(&edited).unwrap_err(), Error::Tampered(1));

    // a line re-hashed after editing breaks the link to its successor
    let mut other = Session::new();
    other.bash("npm run build:evil", Fate::Exit(0));
    let forged = serde_json::to_string(&other.entries()[0]).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    lines[0] = &forged;
    assert_eq!(draft_from_jsonl(&lines.join("\n")).unwrap_err(), Error::Tampered(2));

    // a dropped entry leaves its child without a parent
    let mut dropped: Vec<&str> = text.lines().collect();
    dropped.remove(2);
    assert!(matches!(draft_from_jsonl(&dropped.join("\n")).unwrap_err(), Error::Tampered(_)));

    assert_eq!(draft_from_jsonl("").unwrap_err(), Error::Empty);
    assert!(matches!(draft_from_jsonl("{not json").unwrap_err(), Error::Decode(_)));
}

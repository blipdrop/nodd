//! Nod のCLI。人へ通知し、承認を待つ。
//! 終了コードで判定できるので `nodd ask "本番へ反映" && deploy` のように書ける。

mod api;
mod config;
mod hook;
mod mcp;
mod pairing;

use api::{segment, Api, ApiError, DEFAULT_API};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use config::Profile;
use pairing::PairingKey;
use reqwest::Method;
use serde_json::{json, Map, Value};
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};

const MAX_IMAGE_BYTES: usize = 40 * 1024;
const REJECTED: u8 = 10;
const CLOSED: u8 = 11;
const TIMED_OUT: u8 = 12;

pub const DEFAULT_ASK_SECONDS: u32 = 3600;
const DEFAULT_HOOK_SECONDS: u32 = 300;
const DEFAULT_POLL_SECONDS: u64 = 5;
/// 期限を過ぎても応答の受付猶予がある。サーバーの確定を待ってから諦める。
const AFTER_DEADLINE: chrono::Duration = chrono::Duration::minutes(15);

#[derive(Parser)]
#[command(name = "nodd", version, about = "Send notifications and wait for a human response")]
struct Cli {
    /// Config profile name. Use one per channel when logging in to several
    #[arg(long, global = true, env = "NODD_PROFILE", default_value = "default")]
    profile: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan a QR code on your iPhone to get a send key on this computer
    Login {
        /// API URL
        #[arg(long, env = "NODD_API_URL", default_value = DEFAULT_API)]
        api: String,
        /// Name shown in the app's key list. Defaults to the hostname
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove the key from this computer. Revoke it in the app
    Logout,
    /// Show the current profile and API status
    Whoami,
    /// Send a notification
    Send(Send),
    /// Ask for approval or a choice and wait for the response
    Ask(Ask),
    /// Wait for the response to a sent message
    Wait {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Show the status of a message
    Status { id: String },
    /// Cancel a pending question
    Cancel { id: String },
    /// Report the result of acting on a response
    Done {
        id: String,
        #[arg(long, value_enum, default_value = "succeeded")]
        status: ResultStatus,
        #[arg(long)]
        summary: Option<String>,
    },
    /// Resolve an alert
    Resolve {
        alert_key: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Report that a scheduled job is alive. Notifies if the next report is late
    Heartbeat {
        /// Name of the job, such as `nightly.backup`
        name: String,
        /// Seconds until the next report is expected. 60 to 2678400
        #[arg(long)]
        every: u32,
        /// Extra seconds to wait before notifying. Defaults to 10% of `--every`
        #[arg(long)]
        grace: Option<u32>,
    },
    /// Run an MCP server over stdio for agents
    Mcp,
    /// Claude Code PreToolUse hook. Allows only approved tools
    Hook {
        /// Response deadline in seconds
        #[arg(long, default_value_t = DEFAULT_HOOK_SECONDS)]
        expires_in: u32,
    },
}

#[derive(Args)]
struct Content {
    title: String,
    /// Message body. Use `-` to read from stdin
    #[arg(long)]
    body: Option<String>,
    #[arg(long, value_enum)]
    level: Option<Level>,
    /// `LABEL=VALUE`. Up to 6
    #[arg(long = "field", value_name = "LABEL=VALUE")]
    fields: Vec<String>,
    /// `LABEL=https://…`. Up to 2
    #[arg(long = "link", value_name = "LABEL=URL")]
    links: Vec<String>,
    /// Hide the body on the lock screen
    #[arg(long)]
    sensitive: bool,
    /// Format the body as Markdown. Allows up to 4,000 characters
    #[arg(long)]
    markdown: bool,
    /// Attach a JPEG or PNG file, up to 40 KB
    #[arg(long, value_name = "FILE")]
    image: Option<PathBuf>,
    /// Text describing the image, up to 120 characters
    #[arg(long, requires = "image")]
    image_alt: Option<String>,
    /// Print the result as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct Send {
    #[command(flatten)]
    content: Content,
    #[arg(long, value_enum, default_value = "notice")]
    kind: SendKind,
    /// Group open alerts with the same key. Also used to resolve them
    #[arg(long)]
    alert_key: Option<String>,
}

#[derive(Args)]
struct Ask {
    #[command(flatten)]
    content: Content,
    /// Option as `id=LABEL`. Asks for a choice instead of approval. 2 to 4
    #[arg(long = "option", value_name = "ID=LABEL")]
    options: Vec<String>,
    /// Response deadline in seconds
    #[arg(long, default_value_t = DEFAULT_ASK_SECONDS)]
    expires_in: u32,
    /// People who must approve, 1 to 3. More than 1 needs a Team plan
    #[arg(long)]
    approvals: Option<u32>,
    /// Notify other responders after this many seconds without a response. Needs a Team plan
    #[arg(long)]
    escalate_after: Option<u32>,
    /// Let the responder add text, up to 200 characters. The value is the prompt, up to 40 characters
    #[arg(long, value_name = "PROMPT")]
    input: Option<String>,
    /// Require the text from --input
    #[arg(long, requires = "input")]
    input_required: bool,
    /// Print the send result without waiting
    #[arg(long)]
    no_wait: bool,
    /// Ask for a secret such as a password. Prints only the value, which stays encrypted on the server
    #[arg(long, conflicts_with_all = ["options", "input", "approvals", "no_wait", "json"])]
    secret: bool,
    /// Run COMMAND with the secret in this environment variable instead of printing it
    #[arg(long = "env", value_name = "NAME", requires = "secret", requires = "command")]
    env_name: Option<String>,
    /// Command to run with --env, after `--`
    #[arg(last = true, value_name = "COMMAND", requires = "env_name")]
    command: Vec<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Level {
    Low,
    Normal,
    High,
}

#[derive(Clone, Copy, ValueEnum)]
enum SendKind {
    Notice,
    Alert,
}

#[derive(Clone, Copy, ValueEnum)]
enum ResultStatus {
    Succeeded,
    Failed,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Low => "low",
            Level::Normal => "normal",
            Level::High => "high",
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Approved,
    Rejected,
    Chosen(String),
    /// 応答なしで終わった。expired、cancelled、failed のいずれか。
    Closed(String),
}

/// 応答がある間はそれに従い、ない通知は状態が閉じるまで待つ。
pub fn outcome(state: &Value) -> Option<Outcome> {
    if let Some(option) = state["response"]["option_id"].as_str() {
        let approval = state["kind"] == "approval";
        return Some(match option {
            "approve" if approval => Outcome::Approved,
            "reject" if approval => Outcome::Rejected,
            other => Outcome::Chosen(other.to_string()),
        });
    }
    match state["action_state"].as_str() {
        Some(closed @ ("expired" | "cancelled" | "failed")) => Some(Outcome::Closed(closed.to_string())),
        _ => None,
    }
}

fn exit_code(outcome: &Outcome) -> u8 {
    match outcome {
        Outcome::Approved | Outcome::Chosen(_) => 0,
        Outcome::Rejected => REJECTED,
        Outcome::Closed(_) => CLOSED,
    }
}

fn pair(raw: &str, what: &str) -> Result<(String, String), String> {
    raw.split_once('=')
        .filter(|(left, right)| !left.is_empty() && !right.is_empty())
        .map(|(left, right)| (left.to_string(), right.to_string()))
        .ok_or_else(|| format!("{what} must be in the form `left=right`: {raw}"))
}

fn message_body(kind: &str, content: &Content, read_body: impl FnOnce() -> Result<String, String>) -> Result<Map<String, Value>, String> {
    let mut body = Map::new();
    body.insert("kind".into(), kind.into());
    body.insert("title".into(), content.title.clone().into());
    match content.body.as_deref() {
        Some("-") => {
            body.insert("body".into(), read_body()?.into());
        }
        Some(text) => {
            body.insert("body".into(), text.into());
        }
        None => {}
    }
    if content.markdown {
        body.insert("format".into(), "markdown".into());
    }
    if let Some(path) = &content.image {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(format!("{}: images can be up to 40 KB", path.display()));
        }
        let mut image = json!({"data": STANDARD.encode(bytes)});
        if let Some(alt) = &content.image_alt {
            image["alt"] = alt.as_str().into();
        }
        body.insert("image".into(), image);
    }
    if let Some(level) = content.level {
        body.insert("level".into(), level.name().into());
    }
    if !content.fields.is_empty() {
        let fields = content.fields.iter().map(|raw| {
            pair(raw, "--field").map(|(label, value)| json!({"label": label, "value": value}))
        });
        body.insert("fields".into(), fields.collect::<Result<Vec<_>, _>>()?.into());
    }
    if !content.links.is_empty() {
        let links = content.links.iter().map(|raw| {
            pair(raw, "--link").map(|(label, url)| json!({"label": label, "url": url}))
        });
        body.insert("links".into(), links.collect::<Result<Vec<_>, _>>()?.into());
    }
    if content.sensitive {
        body.insert("sensitive".into(), true.into());
    }
    Ok(body)
}

fn stdin_body() -> Result<String, String> {
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text).map_err(|e| e.to_string())?;
    Ok(text.trim_end().to_string())
}

fn host_name() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "nodd-cli".to_string())
}

struct Session {
    api: Api,
    key: String,
}

impl Session {
    fn open(profile: &str) -> Result<Self, String> {
        let saved = config::load(&config::path()?)?.profiles.remove(profile);
        let key = std::env::var("NODD_KEY").ok().filter(|v| !v.is_empty());
        let api = std::env::var("NODD_API_URL").ok().filter(|v| !v.is_empty());
        let (api, key) = match (key, saved) {
            (Some(key), saved) => (api.or(saved.map(|s| s.api)).unwrap_or_else(|| DEFAULT_API.to_string()), key),
            (None, Some(saved)) => (api.unwrap_or(saved.api), saved.key),
            (None, None) => return Err("No key found. Run `nodd login` or set NODD_KEY".to_string()),
        };
        Ok(Self { api: Api::new(api::normalize_base(&api)?)?, key })
    }

    fn post(&self, path: &str, body: Option<&Value>) -> Result<Value, ApiError> {
        // 1回の実行の中で同じキーを使い、再送で二重に届かないようにする
        let idempotency = ulid::Ulid::new().to_string();
        let reply = self.api.call_retrying(Method::POST, path, Some(&self.key), body, Some(&idempotency))?;
        Ok(reply.body)
    }

    /// 値を開けたら暗号文は要らない。消せなくても巡回が消すので、結果は止めない。
    fn take_secret(&self, id: &str, key: &PairingKey) -> Result<String, String> {
        let path = format!("/v1/messages/{id}/secret");
        let reply = self.api.call_retrying(Method::GET, &path, Some(&self.key), None, None).map_err(|e| e.to_string())?;
        let sealed = reply.body["sealed"].as_str().ok_or("Unexpected response format")?;
        let value = key.open_secret(sealed, id).map_err(|_| "The received secret could not be opened".to_string())?;
        if let Err(error) = self.api.call_retrying(Method::DELETE, &path, Some(&self.key), None, None) {
            eprintln!("The encrypted value stays on the server for up to 10 minutes: {error}");
        }
        Ok(value)
    }

    fn state(&self, id: &str) -> Result<api::Reply, ApiError> {
        self.api.call(Method::GET, &format!("/v1/messages/{id}"), Some(&self.key), None, None)
    }

    /// 通信の失敗では諦めず、期限までは確定した応答だけを結果にする。
    fn wait(&self, id: &str) -> Result<Result<(Outcome, Value), u8>, String> {
        let mut deadline: Option<DateTime<Utc>> = None;
        let mut pause = DEFAULT_POLL_SECONDS;
        let started = Instant::now();
        loop {
            match self.state(id) {
                Ok(reply) => {
                    if let Some(outcome) = outcome(&reply.body) {
                        return Ok(Ok((outcome, reply.body)));
                    }
                    deadline = reply.body["expires_at"]
                        .as_str()
                        .and_then(|at| at.parse().ok())
                        .or(deadline);
                    if reply.body["action_state"].is_null() {
                        return Err("This message does not ask for a response".to_string());
                    }
                    pause = reply.retry_after.unwrap_or(DEFAULT_POLL_SECONDS).max(1);
                }
                Err(error) if error.is_transient() => {
                    eprintln!("{error}. Retrying");
                }
                Err(error) => return Err(error.to_string()),
            }
            let expired = match deadline {
                Some(at) => Utc::now() > at + AFTER_DEADLINE,
                None => started.elapsed() > Duration::from_secs(600),
            };
            if expired {
                eprintln!("Timed out before the response was confirmed");
                return Ok(Err(TIMED_OUT));
            }
            sleep(Duration::from_secs(pause));
        }
    }
}

impl mcp::Backend for Session {
    fn send(&self, body: &Value) -> Result<Value, String> {
        self.post("/v1/messages", Some(body)).map_err(|e| e.to_string())
    }

    fn state(&self, id: &str) -> Result<Value, String> {
        Session::state(self, id).map(|reply| reply.body).map_err(|e| e.to_string())
    }
}

fn hook_decision(profile: &str, expires_in: u32) -> Result<Option<(Outcome, Value)>, String> {
    let input: Value = serde_json::from_str(&stdin_body()?).map_err(|e| e.to_string())?;
    let session = Session::open(profile)?;
    let accepted = session.post("/v1/messages", Some(&hook::message(&input, expires_in))).map_err(|e| e.to_string())?;
    let id = accepted["id"].as_str().ok_or("Unexpected response format")?;
    Ok(session.wait(id)?.ok())
}

/// 応答に添えられた理由。
fn note(state: &Value) -> Option<&str> {
    state["response"]["note"].as_str()
}

fn report(outcome: &Outcome, state: &Value, as_json: bool) -> u8 {
    if as_json {
        println!("{state}");
    } else {
        match outcome {
            Outcome::Approved => println!("approved"),
            Outcome::Rejected => println!("rejected"),
            Outcome::Chosen(option) => println!("{option}"),
            Outcome::Closed(state) => println!("{state}"),
        }
        // 標準出力は判定に使われるので、理由は分けて出す
        if let Some(note) = note(state) {
            eprintln!("Note: {note}");
        }
    }
    exit_code(outcome)
}

fn login(profile: &str, api: &str, name: Option<String>) -> Result<u8, String> {
    let base = api::normalize_base(api)?;
    let client = Api::new(base.clone())?;
    let key = PairingKey::generate();
    let body = json!({"public_key": key.public_base64()});
    let started = client
        .call_retrying(Method::POST, "/v1/cli/pairings", None, Some(&body), None)
        .map_err(|e| e.to_string())?
        .body;
    let text = |name: &str| started[name].as_str().map(str::to_string).ok_or("Unexpected response format");
    let (code, poll_token) = (text("code")?, text("poll_token")?);
    let name = name.unwrap_or_else(host_name);
    println!("Scan this QR code with the Nodd app.\n");
    println!("{}", pairing::qr(&pairing::link(&code, &name))?);
    println!("Code: {code}");
    println!("Name: {name}\n");
    let limit = Instant::now() + Duration::from_secs(started["expires_in"].as_u64().unwrap_or(600));
    let path = format!("/v1/cli/pairings/{code}");
    while Instant::now() < limit {
        let wait = match client.call(Method::GET, &path, Some(&poll_token), None, None) {
            Ok(reply) => {
                if let Some(sealed) = reply.body["sealed"].as_str() {
                    let value = key.open(sealed, &code)?;
                    if !value.starts_with("nodd_") {
                        return Err("The received key is malformed".to_string());
                    }
                    let file = config::path()?;
                    let mut saved = config::load(&file)?;
                    saved.profiles.insert(profile.to_string(), Profile { api: base, key: value });
                    config::save(&file, &saved)?;
                    println!("Logged in. {}", file.display());
                    return Ok(0);
                }
                reply.retry_after.unwrap_or(2).max(1)
            }
            Err(error) if error.is_transient() => 5,
            Err(error) => return Err(error.to_string()),
        };
        sleep(Duration::from_secs(wait));
    }
    Err("Not approved within 10 minutes. Run the command again".to_string())
}

fn logout(profile: &str) -> Result<u8, String> {
    let file = config::path()?;
    let mut saved = config::load(&file)?;
    if saved.profiles.remove(profile).is_none() {
        println!("{profile} is not logged in");
        return Ok(0);
    }
    config::save(&file, &saved)?;
    println!("Removed the key from this computer. Revoke it in the app's channel settings");
    Ok(0)
}

fn whoami(profile: &str) -> Result<u8, String> {
    let session = Session::open(profile)?;
    let meta = session.api.call(Method::GET, "/v1/meta", None, None, None).map_err(|e| e.to_string())?;
    let tail: String = session.key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    println!("profile: {profile}");
    println!("key: …{tail}");
    println!("mode: {}", meta.body["mode"].as_str().unwrap_or("unknown"));
    Ok(0)
}

fn send(profile: &str, command: Send) -> Result<u8, String> {
    let session = Session::open(profile)?;
    let kind = match command.kind {
        SendKind::Notice => "notice",
        SendKind::Alert => "alert",
    };
    let mut body = message_body(kind, &command.content, stdin_body)?;
    if let Some(key) = command.alert_key {
        body.insert("alert_key".into(), key.into());
    }
    let accepted = session.post("/v1/messages", Some(&Value::Object(body))).map_err(|e| e.to_string())?;
    if command.content.json {
        println!("{accepted}");
    } else {
        println!("{}", accepted["id"].as_str().unwrap_or_default());
    }
    Ok(0)
}

fn ask(profile: &str, command: Ask) -> Result<u8, String> {
    if command.secret && command.env_name.is_none() && std::io::stdout().is_terminal() {
        return Err("--secret would show the value on screen. Pipe it, or use --env NAME -- COMMAND".to_string());
    }
    let session = Session::open(profile)?;
    let key = command.secret.then(PairingKey::generate);
    let kind = if command.options.is_empty() { "approval" } else { "choice" };
    let mut body = message_body(kind, &command.content, stdin_body)?;
    body.insert("expires_in".into(), command.expires_in.into());
    if let Some(key) = &key {
        body.insert("secret".into(), json!({"public_key": key.public_base64()}));
    }
    if let Some(count) = command.approvals {
        body.insert("approvals_required".into(), count.into());
    }
    if let Some(seconds) = command.escalate_after {
        body.insert("escalate_after".into(), seconds.into());
    }
    if let Some(prompt) = &command.input {
        body.insert("input".into(), json!({"prompt": prompt, "required": command.input_required}));
    }
    if !command.options.is_empty() {
        let options = command.options.iter().map(|raw| {
            pair(raw, "--option").map(|(id, label)| json!({"id": id, "label": label}))
        });
        body.insert("options".into(), options.collect::<Result<Vec<_>, _>>()?.into());
    }
    let accepted = session.post("/v1/messages", Some(&Value::Object(body))).map_err(|e| e.to_string())?;
    let id = accepted["id"].as_str().ok_or("Unexpected response format")?.to_string();
    if command.no_wait {
        if command.content.json {
            println!("{accepted}");
        } else {
            println!("{id}");
        }
        return Ok(0);
    }
    eprintln!("Waiting for a response: {id}");
    match key {
        Some(key) => finish_secret(&session, &id, &key, command.env_name.as_deref(), &command.command),
        None => finish_wait(&session, &id, command.content.json),
    }
}

/// 標準出力は値の受け渡しに使われるので、値のほかは何も出さない。
fn finish_secret(session: &Session, id: &str, key: &PairingKey, env_name: Option<&str>, command: &[String]) -> Result<u8, String> {
    let outcome = match session.wait(id)? {
        Ok((outcome, _)) => outcome,
        Err(code) => return Ok(code),
    };
    if outcome != Outcome::Approved {
        eprintln!("No secret was returned: {outcome:?}");
        return Ok(exit_code(&outcome));
    }
    let value = session.take_secret(id, key)?;
    let (Some(name), Some((program, args))) = (env_name, command.split_first()) else {
        println!("{value}");
        return Ok(0);
    };
    let status = std::process::Command::new(program).args(args).env(name, value).status().map_err(|e| format!("{program}: {e}"))?;
    Ok(status.code().map_or(1, |code| code as u8))
}

fn finish_wait(session: &Session, id: &str, as_json: bool) -> Result<u8, String> {
    Ok(match session.wait(id)? {
        Ok((outcome, state)) => report(&outcome, &state, as_json),
        Err(code) => code,
    })
}

fn run(cli: Cli) -> Result<u8, String> {
    let profile = cli.profile.as_str();
    match cli.command {
        Command::Mcp => {
            let session = Session::open(profile)?;
            mcp::serve(&session, std::io::stdin().lock(), std::io::stdout().lock())?;
            Ok(0)
        }
        Command::Hook { expires_in } => {
            let result = hook_decision(profile, expires_in);
            let reason = result.as_ref().ok().and_then(Option::as_ref).and_then(|(_, state)| note(state)).map(str::to_string);
            println!("{}", hook::decide(result.map(|r| r.map(|(outcome, _)| outcome)), reason.as_deref()));
            Ok(0)
        }
        Command::Login { api, name } => login(profile, &api, name),
        Command::Logout => logout(profile),
        Command::Whoami => whoami(profile),
        Command::Send(command) => send(profile, command),
        Command::Ask(command) => ask(profile, command),
        Command::Wait { id, json } => finish_wait(&Session::open(profile)?, segment(&id)?, json),
        Command::Status { id } => {
            let state = Session::open(profile)?.state(segment(&id)?).map_err(|e| e.to_string())?;
            println!("{}", state.body);
            Ok(0)
        }
        Command::Cancel { id } => {
            let path = format!("/v1/messages/{}/cancel", segment(&id)?);
            Session::open(profile)?.post(&path, None).map_err(|e| e.to_string())?;
            Ok(0)
        }
        Command::Done { id, status, summary } => {
            let status = match status {
                ResultStatus::Succeeded => "succeeded",
                ResultStatus::Failed => "failed",
            };
            let mut body = json!({"status": status});
            if let Some(summary) = summary {
                body["summary"] = summary.into();
            }
            let path = format!("/v1/messages/{}/result", segment(&id)?);
            Session::open(profile)?.post(&path, Some(&body)).map_err(|e| e.to_string())?;
            Ok(0)
        }
        Command::Heartbeat { name, every, grace } => {
            let mut body = json!({"interval": every});
            if let Some(grace) = grace {
                body["grace"] = grace.into();
            }
            let path = format!("/v1/heartbeats/{}", segment(&name)?);
            let reply = Session::open(profile)?.post(&path, Some(&body)).map_err(|e| e.to_string())?;
            println!("{}", reply["due_at"].as_str().unwrap_or_default());
            Ok(0)
        }
        Command::Resolve { alert_key, note } => {
            let body = note.map(|note| json!({"note": note}));
            let path = format!("/v1/alerts/{}/resolve", segment(&alert_key)?);
            Session::open(profile)?.post(&path, body.as_ref()).map_err(|e| e.to_string())?;
            Ok(0)
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("nod: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_image_is_refused_before_sending() {
        let path = std::env::temp_dir().join(format!("nodd-large-{}.png", std::process::id()));
        std::fs::write(&path, vec![0; MAX_IMAGE_BYTES + 1]).unwrap();
        let content = Content { image: Some(path.clone()), ..content() };
        let error = message_body("notice", &content, || unreachable!()).unwrap_err();
        std::fs::remove_file(path).unwrap();
        assert!(error.contains("40 KB"), "{error}");
    }

    fn content() -> Content {
        Content {
            title: "t".into(),
            body: None,
            level: None,
            fields: vec![],
            links: vec![],
            sensitive: false,
            markdown: false,
            image: None,
            image_alt: None,
            json: false,
        }
    }

    #[test]
    fn only_a_recorded_approval_passes() {
        let approved = json!({"kind": "approval", "action_state": "claimed", "response": {"option_id": "approve"}});
        assert_eq!(outcome(&approved), Some(Outcome::Approved));
        assert_eq!(exit_code(&Outcome::Approved), 0);
        let rejected = json!({"kind": "approval", "response": {"option_id": "reject"}});
        assert_eq!(exit_code(&outcome(&rejected).unwrap()), REJECTED);
        for closed in ["expired", "cancelled", "failed"] {
            let state = json!({"kind": "approval", "action_state": closed});
            assert_eq!(exit_code(&outcome(&state).unwrap()), CLOSED);
        }
        assert_eq!(outcome(&json!({"kind": "approval", "action_state": "open"})), None);
    }

    #[test]
    fn a_choice_named_reject_is_not_a_rejection() {
        let state = json!({"kind": "choice", "response": {"option_id": "reject"}});
        assert_eq!(outcome(&state), Some(Outcome::Chosen("reject".into())));
    }

    #[test]
    fn body_from_flags() {
        let mut input = content();
        input.body = Some("-".into());
        input.level = Some(Level::High);
        input.fields = vec!["環境=prod=eu".into()];
        input.links = vec!["PR=https://example.com/1".into()];
        input.sensitive = true;
        let body = message_body("notice", &input, || Ok("piped".into())).unwrap();
        assert_eq!(body["body"], "piped");
        assert_eq!(body["level"], "high");
        assert_eq!(body["fields"][0]["value"], "prod=eu");
        assert_eq!(body["links"][0]["url"], "https://example.com/1");
        assert_eq!(body["sensitive"], true);
    }

    #[test]
    fn malformed_pairs_are_refused() {
        let mut input = content();
        input.fields = vec!["no-separator".into()];
        assert!(message_body("notice", &input, || unreachable!()).is_err());
        assert!(pair("=x", "--option").is_err());
    }

    #[test]
    fn secret_flags_depend_on_each_other() {
        let parse = |args: &[&str]| Cli::try_parse_from([&["nodd", "ask", "t"], args].concat()).map(|_| ());
        assert!(parse(&["--secret"]).is_ok());
        assert!(parse(&["--secret", "--env", "TOKEN", "--", "deploy", "--now"]).is_ok());
        assert!(parse(&["--secret", "--env", "TOKEN"]).is_err(), "コマンドなしでは値の行き先がない");
        assert!(parse(&["--env", "TOKEN", "--", "deploy"]).is_err());
        assert!(parse(&["--secret", "--", "deploy"]).is_err());
        assert!(parse(&["--secret", "--no-wait"]).is_err(), "鍵はこの実行の中にしかない");
        assert!(parse(&["--secret", "--json"]).is_err());
        assert!(parse(&["--secret", "--input", "p"]).is_err());
    }

    #[test]
    fn commands_parse() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}

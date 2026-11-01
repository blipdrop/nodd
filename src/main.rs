//! Nod のCLI。人へ通知し、承認を待つ。
//! 終了コードで判定できるので `nodd ask "本番へ反映" && deploy` のように書ける。

mod api;
mod config;
mod hook;
mod mcp;
mod pairing;

use api::{segment, Api, ApiError, DEFAULT_API};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use config::Profile;
use pairing::PairingKey;
use reqwest::Method;
use serde_json::{json, Map, Value};
use std::io::Read;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};

const REJECTED: u8 = 10;
const CLOSED: u8 = 11;
const TIMED_OUT: u8 = 12;

pub const DEFAULT_ASK_SECONDS: u32 = 3600;
const DEFAULT_HOOK_SECONDS: u32 = 300;
const DEFAULT_POLL_SECONDS: u64 = 5;
/// 期限を過ぎても応答の受付猶予がある。サーバーの確定を待ってから諦める。
const AFTER_DEADLINE: chrono::Duration = chrono::Duration::minutes(15);

#[derive(Parser)]
#[command(name = "nodd", version, about = "通知を送り、人の応答を待つ")]
struct Cli {
    /// 使う設定の名前。チャンネルごとにログインするときに分ける
    #[arg(long, global = true, env = "NODD_PROFILE", default_value = "default")]
    profile: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// iPhoneでQRを読み、このPCに送信キーを受け取る
    Login {
        /// 接続先
        #[arg(long, env = "NODD_API_URL", default_value = DEFAULT_API)]
        api: String,
        /// アプリのキー一覧に出す名前。既定はホスト名
        #[arg(long)]
        name: Option<String>,
    },
    /// このPCのキーを消す。サーバー側の失効はアプリから行う
    Logout,
    /// 使っている設定と接続先の状態を表示する
    Whoami,
    /// 通知を送る
    Send(Send),
    /// 承認や選択を求め、応答を待つ
    Ask(Ask),
    /// 送信済みの応答を待つ
    Wait {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// 通知の状態を表示する
    Status { id: String },
    /// 応答待ちを取り下げる
    Cancel { id: String },
    /// 応答を処理した結果を報告する
    Done {
        id: String,
        #[arg(long, value_enum, default_value = "succeeded")]
        status: ResultStatus,
        #[arg(long)]
        summary: Option<String>,
    },
    /// アラートを解消する
    Resolve {
        alert_key: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// エージェントから使うMCPサーバーを標準入出力で動かす
    Mcp,
    /// Claude CodeのPreToolUseフック。承認されたツールだけを通す
    Hook {
        /// 応答期限の秒数
        #[arg(long, default_value_t = DEFAULT_HOOK_SECONDS)]
        expires_in: u32,
    },
}

#[derive(Args)]
struct Content {
    title: String,
    /// 本文。`-` で標準入力から読む
    #[arg(long)]
    body: Option<String>,
    #[arg(long, value_enum)]
    level: Option<Level>,
    /// `ラベル=値`。6個まで
    #[arg(long = "field", value_name = "LABEL=VALUE")]
    fields: Vec<String>,
    /// `ラベル=https://…`。2個まで
    #[arg(long = "link", value_name = "LABEL=URL")]
    links: Vec<String>,
    /// ロック画面に本文を出さない
    #[arg(long)]
    sensitive: bool,
    /// 結果をJSONで出す
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct Send {
    #[command(flatten)]
    content: Content,
    #[arg(long, value_enum, default_value = "notice")]
    kind: SendKind,
    /// 同じキーの未解決アラートを1件にまとめ、解消にも使う
    #[arg(long)]
    alert_key: Option<String>,
}

#[derive(Args)]
struct Ask {
    #[command(flatten)]
    content: Content,
    /// 選択肢 `id=ラベル`。指定すると承認ではなく選択を求める。2〜4個
    #[arg(long = "option", value_name = "ID=LABEL")]
    options: Vec<String>,
    /// 応答期限の秒数
    #[arg(long, default_value_t = DEFAULT_ASK_SECONDS)]
    expires_in: u32,
    /// 待たずに送信結果だけを出す
    #[arg(long)]
    no_wait: bool,
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
        .ok_or_else(|| format!("{what} は `左=右` の形で指定してください: {raw}"))
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
            (None, None) => return Err("キーがありません。`nodd login` を実行するか NODD_KEY を設定してください".to_string()),
        };
        Ok(Self { api: Api::new(api::normalize_base(&api)?)?, key })
    }

    fn post(&self, path: &str, body: Option<&Value>) -> Result<Value, ApiError> {
        // 1回の実行の中で同じキーを使い、再送で二重に届かないようにする
        let idempotency = ulid::Ulid::new().to_string();
        let reply = self.api.call_retrying(Method::POST, path, Some(&self.key), body, Some(&idempotency))?;
        Ok(reply.body)
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
                        return Err("この通知は応答を求めていません".to_string());
                    }
                    pause = reply.retry_after.unwrap_or(DEFAULT_POLL_SECONDS).max(1);
                }
                Err(error) if error.is_transient() => {
                    eprintln!("{error}。再試行します");
                }
                Err(error) => return Err(error.to_string()),
            }
            let expired = match deadline {
                Some(at) => Utc::now() > at + AFTER_DEADLINE,
                None => started.elapsed() > Duration::from_secs(600),
            };
            if expired {
                eprintln!("応答を確定できないまま待ち時間を過ぎました");
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

fn hook_decision(profile: &str, expires_in: u32) -> Result<Option<Outcome>, String> {
    let input: Value = serde_json::from_str(&stdin_body()?).map_err(|e| e.to_string())?;
    let session = Session::open(profile)?;
    let accepted = session.post("/v1/messages", Some(&hook::message(&input, expires_in))).map_err(|e| e.to_string())?;
    let id = accepted["id"].as_str().ok_or("応答の形式が正しくありません")?;
    Ok(session.wait(id)?.ok().map(|(outcome, _)| outcome))
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
    let text = |name: &str| started[name].as_str().map(str::to_string).ok_or("応答の形式が正しくありません");
    let (code, poll_token) = (text("code")?, text("poll_token")?);
    let name = name.unwrap_or_else(host_name);
    println!("Noddアプリの「CLIを追加」でこのQRを読み取ってください。\n");
    println!("{}", pairing::qr(&pairing::link(&code, &name))?);
    println!("コード: {code}");
    println!("名前: {name}\n");
    let limit = Instant::now() + Duration::from_secs(started["expires_in"].as_u64().unwrap_or(600));
    let path = format!("/v1/cli/pairings/{code}");
    while Instant::now() < limit {
        let wait = match client.call(Method::GET, &path, Some(&poll_token), None, None) {
            Ok(reply) => {
                if let Some(sealed) = reply.body["sealed"].as_str() {
                    let value = key.open(sealed, &code)?;
                    if !value.starts_with("nodd_") {
                        return Err("受け取ったキーの形式が正しくありません".to_string());
                    }
                    let file = config::path()?;
                    let mut saved = config::load(&file)?;
                    saved.profiles.insert(profile.to_string(), Profile { api: base, key: value });
                    config::save(&file, &saved)?;
                    println!("ログインしました。{}", file.display());
                    return Ok(0);
                }
                reply.retry_after.unwrap_or(2).max(1)
            }
            Err(error) if error.is_transient() => 5,
            Err(error) => return Err(error.to_string()),
        };
        sleep(Duration::from_secs(wait));
    }
    Err("10分以内に許可されませんでした。もう一度実行してください".to_string())
}

fn logout(profile: &str) -> Result<u8, String> {
    let file = config::path()?;
    let mut saved = config::load(&file)?;
    if saved.profiles.remove(profile).is_none() {
        println!("{profile} はログインしていません");
        return Ok(0);
    }
    config::save(&file, &saved)?;
    println!("このPCのキーを消しました。キーの失効はアプリのチャンネル設定から行ってください");
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
    let session = Session::open(profile)?;
    let kind = if command.options.is_empty() { "approval" } else { "choice" };
    let mut body = message_body(kind, &command.content, stdin_body)?;
    body.insert("expires_in".into(), command.expires_in.into());
    if !command.options.is_empty() {
        let options = command.options.iter().map(|raw| {
            pair(raw, "--option").map(|(id, label)| json!({"id": id, "label": label}))
        });
        body.insert("options".into(), options.collect::<Result<Vec<_>, _>>()?.into());
    }
    let accepted = session.post("/v1/messages", Some(&Value::Object(body))).map_err(|e| e.to_string())?;
    let id = accepted["id"].as_str().ok_or("応答の形式が正しくありません")?.to_string();
    if command.no_wait {
        if command.content.json {
            println!("{accepted}");
        } else {
            println!("{id}");
        }
        return Ok(0);
    }
    eprintln!("応答を待っています: {id}");
    finish_wait(&session, &id, command.content.json)
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
            println!("{}", hook::decide(hook_decision(profile, expires_in)));
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

    fn content() -> Content {
        Content {
            title: "t".into(),
            body: None,
            level: None,
            fields: vec![],
            links: vec![],
            sensitive: false,
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
    fn commands_parse() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}

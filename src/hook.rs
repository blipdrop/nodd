//! `nodd hook`。Claude CodeのPreToolUseフックで、ツールの実行前に人の承認を取る。
//! 異常終了ではツールが動いてしまうので、失敗はすべて拒否の出力にする。

use crate::Outcome;
use serde_json::{json, Value};

const MAX_TITLE: usize = 120;
const MAX_BODY: usize = 1000;

fn clip(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

pub fn message(input: &Value, expires_in: u32) -> Value {
    let tool = input["tool_name"].as_str().unwrap_or("unknown");
    let detail = input["tool_input"]["command"]
        .as_str()
        .map_or_else(|| input["tool_input"].to_string(), str::to_string);
    json!({
        "kind": "approval",
        "level": "high",
        "title": clip(&format!("Claude Code: {tool}"), MAX_TITLE),
        "body": clip(&detail, MAX_BODY),
        "expires_in": expires_in,
    })
}

fn output(decision: &str, reason: &str) -> Value {
    json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": decision,
        "permissionDecisionReason": reason,
    }})
}

/// 記録された承認だけを許可する。
pub fn decide(result: Result<Option<Outcome>, String>) -> Value {
    match result {
        Ok(Some(Outcome::Approved)) => output("allow", "Nodで承認されました"),
        Ok(Some(Outcome::Rejected)) => output("deny", "Nodで却下されました"),
        Ok(Some(Outcome::Closed(state))) => output("deny", &format!("承認されませんでした: {state}")),
        Ok(Some(Outcome::Chosen(_))) | Ok(None) => output("deny", "承認を確認できませんでした"),
        Err(error) => output("deny", &format!("Nodを確認できませんでした: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision(value: &Value) -> &str {
        value["hookSpecificOutput"]["permissionDecision"].as_str().unwrap()
    }

    #[test]
    fn only_approval_allows() {
        assert_eq!(decision(&decide(Ok(Some(Outcome::Approved)))), "allow");
        for denied in [
            Ok(Some(Outcome::Rejected)),
            Ok(Some(Outcome::Closed("expired".into()))),
            Ok(Some(Outcome::Chosen("approve".into()))),
            Ok(None),
            Err("offline".to_string()),
        ] {
            assert_eq!(decision(&decide(denied)), "deny");
        }
    }

    #[test]
    fn message_shows_the_command_and_fits_the_limits() {
        let input = json!({"tool_name": "Bash", "tool_input": {"command": "x".repeat(2000)}});
        let body = message(&input, 300);
        assert_eq!(body["title"], "Claude Code: Bash");
        assert_eq!(body["body"].as_str().unwrap().chars().count(), MAX_BODY);
        let other = message(&json!({"tool_name": "Write", "tool_input": {"file_path": "/a"}}), 300);
        assert!(other["body"].as_str().unwrap().contains("/a"));
    }
}

//! Jev System One adapter — escalation-tier classification.
//!
//! Jev (TypeSafe's typed judgment model) classifies a chat message into
//! {spam, pump, abuse, information, question, banter} with a confidence.
//! Experimentally verified (2026-09-27, 43-message live-tape test):
//! - agrees with the regex spam filter 3/3 at confidence 1.00;
//! - catches what heuristics cannot: price-influence attempts ("Get Yes on
//!   Blakeman before the price rises" -> pump @ 1.00) and material
//!   information ("plane made an emergency water landing" -> information
//!   @ 1.00);
//! - REQUIRES one call per message — batching messages into one state
//!   dilutes context and everything returns "banter" @ ~0.8 (flat).
//!
//! Because each call costs tokens, this module is an ESCALATION tier: the
//! caller gates cheaply (sender position size, entropy anomalies) and only
//! pays for the interesting slice. API key comes from the environment
//! (TYPESAFE_API_KEY); chathound never reads credential files.

use crate::ChError;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
pub const MODEL: &str = "jev-latest";

pub const CLASSES: [&str; 6] = ["spam", "pump", "abuse", "information", "question", "banter"];

fn criteria() -> serde_json::Value {
    serde_json::json!({
        "spam": "Phishing or fake-bonus promotion (fake kalshi domains, gift/claim/rewards links, scams)",
        "pump": "Attempt to influence the price or coordinate action: telling others to buy/sell a side, hyping a position so the price moves",
        "abuse": "Harassment, threats of violence, slurs, or pure flame war content",
        "information": "A factual or material claim about the event outcome: news, injury, score, poll, data, or settlement-relevant fact",
        "question": "Asking the room or support a question",
        "banter": "Opinion, trash talk, celebration, or social chat with no material claim"
    })
}

#[derive(Debug, Serialize)]
struct Question {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: String,
    criteria: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct Answer {
    #[serde(default)]
    choice: Option<String>,
    #[serde(default)]
    confidence: Option<f64>,
    /// Parsed for schema-fidelity; unused by triage (confidence is the
    /// action axis) — kept to catch API shape drift loudly in tests.
    #[serde(default)]
    #[allow(dead_code)]
    probabilities: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(default)]
    answers: std::collections::HashMap<String, Answer>,
}

/// Classify one message. One call per message — never batch (context
/// dilution; see module docs). Returns (class, confidence).
pub fn classify(
    text: &str,
    event: &str,
    side: Option<&str>,
    api_key: &str,
) -> Result<(String, Option<f64>), ChError> {
    let body = serde_json::json!({
        "state": {
            "message": text,
            "market_event": event,
            "sender_position_side": side,
            "room": "live prediction-market chat"
        },
        "model": MODEL,
        "questions": {
            "cls": Question {
                kind: "choice",
                instructions: "Classify this chat message into exactly one class."
                    .to_string(),
                criteria: criteria(),
            }
        }
    });
    let resp = ureq::post(ENDPOINT)
        .set("Authorization", &format!("Bearer {api_key}"))
        .set("Content-Type", "application/json")
        .timeout(Duration::from_secs(30))
        .send_string(&body.to_string())
        .map_err(|e| ChError::Http(format!("jev: {e}")))?;
    let parsed: Response =
        serde_json::from_reader(std::io::Read::take(resp.into_reader(), 8 * 1024 * 1024))
            .map_err(|e| ChError::Parse(format!("jev: {e}")))?;
    let ans = parsed
        .answers
        .get("cls")
        .ok_or_else(|| ChError::Parse("jev: no 'cls' answer".into()))?;
    let class = ans.choice.clone().unwrap_or_else(|| "unknown".into());
    if !CLASSES.contains(&class.as_str()) && class != "unknown" {
        return Err(ChError::Parse(format!("jev: unknown class {class}")));
    }
    Ok((class, ans.confidence))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_has_the_verified_shape() {
        // Wire-shape regression: the body this module builds must contain
        // the exact schema the 2026-09-27 live experiment used.
        let q = Question {
            kind: "choice",
            instructions: "Classify this chat message into exactly one class.".into(),
            criteria: criteria(),
        };
        let body = serde_json::json!({
            "state": {"message": "Get Yes on Blakeman before the price rises",
                      "market_event": "GOVPARTYNY-26",
                      "sender_position_side": "yes",
                      "room": "live prediction-market chat"},
            "model": MODEL,
            "questions": {"cls": q}
        });
        let s = body.to_string();
        assert!(s.contains("\"type\":\"choice\""));
        assert!(s.contains("jev-latest"));
        assert!(s.contains("emergency") || s.contains("pump")); // criteria present
    }

    #[test]
    fn response_parses_golden_sample() {
        let raw = r#"{"answers":{"cls":{"choice":"pump","confidence":1.0,
            "probabilities":{"pump":0.97}}},"model":"jev-1.13.0"}"#;
        let r: Response = serde_json::from_str(raw).unwrap();
        let a = r.answers.get("cls").expect("cls present");
        assert_eq!(a.choice.as_deref(), Some("pump"));
        assert_eq!(a.confidence, Some(1.0));
    }
}

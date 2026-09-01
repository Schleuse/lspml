//! Drives the built server over stdio, which is the only place the position
//! encoding is observable: it is settled once during the handshake and decides
//! what every column on the wire is counted in from there on.

use std::{
    collections::HashMap,
    io::Write,
    process::{Command, Stdio},
};

use serde_json::{Value, json};

/// A file of german prose, whose umlauts put the columns behind them ahead of
/// the bytes the server counts:
///
/// ```text
/// 3 <sp:set value="Grüße" name="gruss"/>
/// 4 <div class="übersicht">Höhe und Breite der Fläche: <sp:print name="gruss"/></div>
/// 5 <div class="größe"><sp:if name="gruss" eq="x">Grüße</sp:if></div>
/// 6 <sp:if condition="${gruss}">Schöne Grüße</sp:if>
/// ```
const GERMAN: &str = include_str!("../fixtures/german.spml");

const URI: &str = "file:///lspml/german.spml";

/// Each anchor is one place in the fixture, counted the way the client counts it
/// and the way the server does: `(code units, bytes)`.
type Anchor = (u64, u64);

/// The `gruss` the `<sp:set>` of line 3 declares, behind the "ü" and the "ß" of
/// the value before it. A go-to-definition answers with it.
const DECLARATION: Anchor = (28, 30);

/// The `gruss` of the `<sp:print>` on line 4, behind three umlauts of prose.
const REFERENCE: Anchor = (67, 70);

/// The `<sp:if>` of line 5, behind the "ö" and the "ß" of its wrapping div.
const IF_TAG: Anchor = (19, 21);

/// The `name="gruss"` of that same `<sp:if>`, which its code action replaces,
/// and the `gruss` inside it, which is a semantic token of its own.
const IF_NAME: Anchor = (26, 28);
const IF_NAME_VALUE: Anchor = (32, 34);

/// The ` eq="x"` behind it, which the same action removes.
const IF_EQ: Anchor = (38, 40);
const IF_EQ_END: Anchor = (45, 47);

/// The `${gruss}` of line 6, which nothing multi byte precedes: the same column
/// in either encoding, so what it points at does not depend on the handshake.
const CONDITION: Anchor = (21, 21);

fn code_units((code_units, _): Anchor) -> u64 {
    return code_units;
}

fn bytes((_, bytes): Anchor) -> u64 {
    return bytes;
}

#[test]
fn test_a_client_counting_code_units_is_answered_in_code_units() {
    let answers = session(&["utf-16"], code_units);

    assert_eq!(answers.encoding(), "utf-16");
    assert_columns(&answers, code_units);
}

#[test]
fn test_a_client_counting_bytes_is_answered_in_bytes() {
    let answers = session(&["utf-8", "utf-16"], bytes);

    assert_eq!(answers.encoding(), "utf-8");
    assert_columns(&answers, bytes);
}

/// Every column of every answer, counted by `column`. The two encodings differ
/// only in how the same places are counted, so both clients expect the same
/// anchors read their own way.
fn assert_columns(answers: &Answers, column: fn(Anchor) -> u64) {
    assert_eq!(answers.definition(), (3, column(DECLARATION)));
    assert_eq!(answers.first_completion_edit(), (4, column(REFERENCE)));
    assert_eq!(
        answers.semantic_tokens(),
        vec![
            (3, column(DECLARATION)),
            (4, column(REFERENCE)),
            (5, column(IF_NAME_VALUE)),
            // The three of line 6 sit in front of that line's umlauts, so they
            // come out the same in either encoding.
            (6, 18),
            (6, 20),
            (6, 25),
        ]
    );
    assert_eq!(
        answers.code_action_edits(),
        vec![
            (5, column(IF_EQ), column(IF_EQ_END)),
            (5, column(IF_NAME), column(IF_EQ)),
        ]
    );
}

/// The cursor is the half of the conversion that is easiest to forget: a
/// position the client counted has to be read back into a byte column before
/// anything can be looked up under it.
#[test]
fn test_a_cursor_the_client_counted_reaches_what_it_points_at() {
    let counted = session(&["utf-16"], code_units);
    assert!(counted.hover().is_some());
    assert_eq!(counted.code_action_edits().len(), 2);

    // The very same columns read as byte ones land two bytes short, inside the
    // "größe" of the attribute before the tag, where there is nothing to answer.
    let uncounted = session(&["utf-16"], |anchor| code_units(anchor) - 2);
    assert!(uncounted.hover().is_none());
    assert_eq!(uncounted.code_action_edits().len(), 0);
}

/// The answers of one server run, by request id.
struct Answers(HashMap<i64, Value>);

impl Answers {
    fn encoding(&self) -> String {
        return self.result(0)["capabilities"]["positionEncoding"]
            .as_str()
            .expect("the handshake declares an encoding")
            .to_string();
    }

    fn definition(&self) -> (u64, u64) {
        return position(&self.result(1)["range"]["start"]);
    }

    fn hover(&self) -> Option<&Value> {
        return match self.result(2) {
            Value::Null => None,
            hover => Some(hover),
        };
    }

    /// The line and starting column of every semantic token, walked out of the
    /// deltas the protocol encodes them as.
    fn semantic_tokens(&self) -> Vec<(u64, u64)> {
        let data = self.result(3)["data"]
            .as_array()
            .expect("the answer carries token data");
        let mut tokens = Vec::new();
        let mut line = 0;
        let mut column = 0;
        for token in data.chunks(5) {
            let delta_line = number(&token[0]);
            line += delta_line;
            column = match delta_line {
                0 => column + number(&token[1]),
                _ => number(&token[1]),
            };
            tokens.push((line, column));
        }
        return tokens;
    }

    /// Every edit of every code action offered, as line, start and end.
    fn code_action_edits(&self) -> Vec<(u64, u64, u64)> {
        return self
            .result(4)
            .as_array()
            .expect("the answer is a list of actions")
            .iter()
            .flat_map(|action| {
                action["edit"]["changes"][URI]
                    .as_array()
                    .expect("an action edits the document")
                    .iter()
            })
            .map(|edit| {
                let (line, start) = position(&edit["range"]["start"]);
                return (line, start, position(&edit["range"]["end"]).1);
            })
            .collect();
    }

    fn first_completion_edit(&self) -> (u64, u64) {
        return position(&self.result(5)[0]["textEdit"]["range"]["start"]);
    }

    fn result(&self, id: i64) -> &Value {
        return self
            .0
            .get(&id)
            .unwrap_or_else(|| panic!("request {} went unanswered", id));
    }
}

fn position(position: &Value) -> (u64, u64) {
    return (number(&position["line"]), number(&position["character"]));
}

fn number(value: &Value) -> u64 {
    return value.as_u64().expect("a position is counted in numbers");
}

/// Opens the fixture on a server that advertises `encodings` and asks it
/// everything it can answer, with every column the requests carry counted by
/// `column`.
fn session(encodings: &[&str], column: fn(Anchor) -> u64) -> Answers {
    let cursor = |line: u32, anchor: Anchor| json!({ "line": line, "character": column(anchor) });
    let messages = vec![
        request(
            0,
            "initialize",
            json!({
                "processId": null,
                "rootUri": null,
                "capabilities": { "general": { "positionEncodings": encodings } },
            }),
        ),
        notification("initialized", json!({})),
        notification(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": URI, "languageId": "spml", "version": 1, "text": GERMAN,
            }}),
        ),
        request(
            1,
            "textDocument/definition",
            json!({
                "textDocument": { "uri": URI },
                "position": { "line": 6, "character": code_units(CONDITION) },
            }),
        ),
        request(
            2,
            "textDocument/hover",
            json!({ "textDocument": { "uri": URI }, "position": cursor(5, IF_TAG) }),
        ),
        request(
            3,
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": URI } }),
        ),
        request(
            4,
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": URI },
                "range": { "start": cursor(5, IF_TAG), "end": cursor(5, IF_TAG) },
                "context": { "diagnostics": [] },
            }),
        ),
        request(
            5,
            "textDocument/completion",
            json!({
                "textDocument": { "uri": URI },
                "position": cursor(4, REFERENCE),
                "context": { "triggerKind": 1 },
            }),
        ),
        request(9, "shutdown", Value::Null),
        notification("exit", json!({})),
    ];
    return Answers(answers(&run(&messages)));
}

fn request(id: i64, method: &str, params: Value) -> Value {
    return json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
}

fn notification(method: &str, params: Value) -> Value {
    return json!({ "jsonrpc": "2.0", "method": method, "params": params });
}

/// Feeds `messages` to a server on its stdin and hands back everything it wrote
/// before exiting. Writing the whole conversation up front works because the
/// server answers requests in the order they arrive.
fn run(messages: &[Value]) -> Vec<u8> {
    let mut input = Vec::new();
    for message in messages {
        let body = serde_json::to_vec(message).expect("a message serializes");
        write!(input, "Content-Length: {}\r\n\r\n", body.len()).expect("writing to a vec");
        input.extend_from_slice(&body);
    }
    let mut server = Command::new(env!("CARGO_BIN_EXE_lspml"))
        .args(["--log-level", "ERROR"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the server should start");
    server
        .stdin
        .take()
        .expect("the server has a stdin")
        .write_all(&input)
        .expect("the server should read its stdin");
    let output = server.wait_with_output().expect("the server should exit");
    assert!(
        output.status.success(),
        "the server exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    return output.stdout;
}

/// Unframes what the server wrote, keeping the result of every answer it gave.
fn answers(output: &[u8]) -> HashMap<i64, Value> {
    let mut answers = HashMap::new();
    let mut rest = output;
    while let Some(offset) = rest.windows(4).position(|window| window == b"\r\n\r\n") {
        let header = std::str::from_utf8(&rest[..offset]).expect("a header is ascii");
        let length = header
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .expect("a framed message states its length")
            .trim()
            .parse::<usize>()
            .expect("a length is a number");
        let body = &rest[offset + 4..offset + 4 + length];
        rest = &rest[offset + 4 + length..];
        let message: Value = serde_json::from_slice(body).expect("a body is json");
        if let Some(id) = message.get("id").and_then(Value::as_i64) {
            answers.insert(id, message["result"].clone());
        }
    }
    return answers;
}

//! Persistent source process protocol. The parent bounds each VM request while
//! keeping the worker/VM alive between requests. No network listener is opened.
use crate::{native, validate_result, Worker};
use serde_json::{json, Value};
use std::{
    env,
    io::{self, BufRead, BufReader, Write},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

const REQUEST_LIMIT: usize = 608 * 1024 * 1024;
const RESPONSE_LIMIT: usize = 1024 * 1024;
// Wall deadline for world init/update ops (see the per-op match below).
pub const WORLD_OP_WALL_SECONDS: u64 = 30;

#[derive(serde::Deserialize)]
struct CommandHeader {
    #[serde(default)]
    op: Option<Value>,
}

fn closes_session(data: &[u8]) -> Result<bool, String> {
    Ok(operation(data)?.as_deref() == Some("close"))
}

fn operation(data: &[u8]) -> Result<Option<String>, String> {
    // The native engine validates the command body. These framing processes
    // only need the operation: retaining a Value for a streamed world update
    // duplicates its entire object tree in both the supervisor and VM worker.
    // Unknown fields are still parsed/validated, but skipped without a DOM.
    let header: CommandHeader =
        serde_json::from_slice(data).map_err(|_| "invalid persistent request JSON")?;
    Ok(header.op.as_ref().and_then(Value::as_str).map(str::to_owned))
}

fn frame<R: BufRead>(reader: &mut R, limit: usize) -> Result<Option<Vec<u8>>, String> {
    let mut output = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|e| e.to_string())?;
        if available.is_empty() {
            return if output.is_empty() {
                Ok(None)
            } else {
                Err("incomplete persistent frame".into())
            };
        }
        let end = available.iter().position(|b| *b == b'\n');
        let count = end.unwrap_or(available.len());
        if output.len() + count > limit {
            return Err("persistent frame exceeds size limit".into());
        }
        output.extend_from_slice(&available[..count]);
        reader.consume(count + usize::from(end.is_some()));
        if end.is_some() {
            return Ok(Some(output));
        }
    }
}
fn emit(value: &Value) -> Result<(), String> {
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, value).map_err(|e| e.to_string())?;
    output
        .write_all(b"\n")
        .and_then(|_| output.flush())
        .map_err(|e| e.to_string())
}

pub fn worker() -> Result<(), String> {
    let input = io::stdin();
    let mut reader = input.lock();
    let mut session = native::Session::new();
    while let Some(data) = frame(&mut reader, REQUEST_LIMIT)? {
        if closes_session(&data)? {
            emit(&json!({"ok":true,"values":[],"closed":true}))?;
            return Ok(());
        }
        let result = session.request(&data)?;
        emit(&validate_result(&result, true)?)?;
    }
    Ok(())
}

pub fn supervise() -> Result<(), String> {
    let child = Command::new(env::current_exe().map_err(|e| e.to_string())?)
        .arg("--session-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut child = Worker(child);
    let mut input = child
        .0
        .stdin
        .take()
        .ok_or("persistent worker has no stdin")?;
    let output = child
        .0
        .stdout
        .take()
        .ok_or("persistent worker has no stdout")?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut reader = BufReader::new(output);
        loop {
            match frame(&mut reader, RESPONSE_LIMIT) {
                Ok(Some(data)) => {
                    if sender.send(Ok(data)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = sender.send(Err("persistent VM worker closed its output".into()));
                    break;
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    break;
                }
            }
        }
    });
    let source = io::stdin();
    let mut reader = source.lock();
    while let Some(data) = frame(&mut reader, REQUEST_LIMIT)? {
        let op = operation(&data)?;
        let closing = op.as_deref() == Some("close");
        input
            .write_all(&data)
            .and_then(|_| input.write_all(b"\n"))
            .and_then(|_| input.flush())
            .map_err(|e| e.to_string())?;
        // World init/update carry replicated state that must be applied whole;
        // large games stream hundreds of KB at join and can exceed the short
        // interactive deadline on a busy host. Scripts/render keep 5 seconds.
        let deadline = match op.as_deref() {
            Some("init") | Some("update") => Duration::from_secs(WORLD_OP_WALL_SECONDS),
            _ => Duration::from_secs(5),
        };
        let response = match receiver.recv_timeout(deadline) {
            Ok(result) => result?,
            Err(_) => {
                emit(
                    &json!({"ok":false,"values":[],"timed_out":true,"state_lost":true,"error":"persistent worker wall time limit exceeded"}),
                )?;
                return Err("persistent worker exceeded its wall deadline".into());
            }
        };
        emit(&validate_result(&response, true)?)?;
        if closing {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn operation_header_skips_world_fields_but_validates_json() {
        assert!(closes_session(br#"{"snapshot":{"op":"update"},"op":"cl\u006fse"}"#).unwrap());
        assert!(!closes_session(br#"{"op":"update","delta":{"actions":[{"op":"close"}]}}"#).unwrap());
        assert!(!closes_session(br#"{"op":null,"source":"close"}"#).unwrap());
        assert!(closes_session(br#"{"op":"update","delta":[invalid]}"#).is_err());
        assert!(closes_session(br#"{"op":"close"} trailing"#).is_err());
        let request = format!(r#"{{"op":"update","delta":{{"text":"{}"}}}}"#, "x".repeat(4 * 1024 * 1024));
        assert!(!closes_session(request.as_bytes()).unwrap());
    }
    #[test]
    fn frames_bound_size_and_require_terminators() {
        assert_eq!(
            frame(&mut &b"one\ntwo\n"[..], 10).unwrap(),
            Some(b"one".to_vec())
        );
        assert!(frame(&mut &b"unfinished"[..], 20).is_err());
        assert!(frame(&mut &b"oversized\n"[..], 3).is_err());
        assert!(frame(&mut &b""[..], 10).unwrap().is_none());
    }
}

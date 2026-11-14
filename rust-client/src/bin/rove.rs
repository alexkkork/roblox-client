//! the rust entry point for the official luau vm and local replays.
//! auth and join only connect when you actually pick those commands.
//! the other modes run locally.
#[cfg(feature = "allocator-mimalloc")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "allocator-mimalloc")]
const RUST_ALLOCATOR_LABEL: &str = "mimalloc";
#[cfg(not(feature = "allocator-mimalloc"))]
const RUST_ALLOCATOR_LABEL: &str = "system";

use headless_luau::engine_world_projection::ProjectionContext;
use headless_luau::engine_world_view::SharedWorldView;
use headless_luau::join_cli;
use headless_luau::live_runtime::{compact_delta_with_candidates, compact_snapshot};
use headless_luau::runtime_owner::{successful, Runtime as Driver};
use headless_luau::session_ui::SessionUi;
use headless_luau::ui_server::{random_ui_key, UiConfig, UiServer};
#[cfg(any(unix, windows))]
use rove_client_core::console::{ScriptConsole, SessionLease};
use rove_client_core::{
    console::send_request,
    memory::current_process_rss_bytes,
    runtime_delta::RuntimeDeltaNormalizer,
    runtime_snapshot::{build_runtime_snapshot, RuntimeSnapshotMetadata},
    session::{IngestOutcome, SessionCore},
    terminal::{Terminal, TerminalEvent},
    world::WorldStore,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, HashSet},
    env,
    fs::{self, File},
    io::{self, BufRead, BufReader, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};

const SNAPSHOT_LIMIT: usize = 8 * 1024 * 1024;
const SOURCE_LIMIT: usize = 100 * 1024 * 1024;
const USAGE: &str = "rove run --snapshot FILE --script FILE [--deltas JSONL] [--lua-memory-mib 32] [--timeout-seconds 60] [--hold-ms 0] [--json]\nrove replay --packets FILE --script FILE [--json]\nrove replay-stream --packets FILE --script FILE [--packet-interval-ms 0] [--json]\nrove repl --snapshot FILE [--lua-memory-mib 32]\nrove console --snapshot FILE --socket PATH --lease-root PATH [--lua-memory-mib 32]\nrove serve --snapshot FILE [--port 5467] [--socket PATH --lease-root PATH] [--ui-root PATH] [--content-root PATH] [--script FILE] [--serve-seconds 0]\nrove submit --socket PATH --script FILE\nrove status --socket PATH --job-id ID\nrove leave --socket PATH\nrove snapshot --snapshot FILE [--deltas JSONL]\nrove auth [--credential-file FILE]\nrove join (--place-id ID [--job-id UUID] | --private-link-file FILE | --owned-private-server-file FILE) [--credential-file FILE] [--transport-seconds 1..30 | --session-seconds 1..3600 [--physics] [--lightweight] [--script FILE [--lua-memory-mib 32]]]\nSession mode owns live reception in Rust; --script runs authored Luau against ordered received updates. The selected engine host embeds the official Luau VM; --physics enables own-character gravity and movement.\n--lightweight omits received visual/audio asset properties (reads report unavailable) and uses a 1 MiB cold-property cache backed by a private temporary file; physics and gameplay state remain.\nThe Lua memory limit is an allocator cap, not a total-process RAM cap.";

// json! borrows its inputs, so an existing Value or String gets cloned.
// move the big inputs in ourselves so we do not keep two copies.
fn source_command(source: String, timeout_ms: u64) -> Value {
    let mut command = json!({"op":"execute", "timeout_ms":timeout_ms});
    command["source"] = Value::String(source);
    command
}

fn update_command(delta: Value, timeout_ms: u64) -> Value {
    let mut command = json!({"op":"update", "timeout_ms":timeout_ms});
    command["delta"] = delta;
    command
}

#[derive(Debug)]
struct Options {
    mode: String,
    snapshot: PathBuf,
    packets: Option<PathBuf>,
    script: Option<PathBuf>,
    deltas: Option<PathBuf>,
    socket: Option<PathBuf>,
    lease_root: Option<PathBuf>,
    memory: u32,
    timeout: u64,
    hold_ms: u64,
    packet_interval_ms: u64,
    json: bool,
    job_id: Option<u64>,
    ui_root: Option<PathBuf>,
    content_root: Option<PathBuf>,
    port: u16,
    serve_seconds: u64,
}

fn parse(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut args = args;
    let mode = args.next().ok_or(USAGE)?;
    if !matches!(
        mode.as_str(),
        "run"
            | "replay"
            | "replay-stream"
            | "world-replay"
            | "repl"
            | "console"
            | "serve"
            | "snapshot"
            | "submit"
            | "status"
            | "leave"
    ) {
        return Err(USAGE.into());
    }
    let mut options = Options {
        mode,
        snapshot: PathBuf::new(),
        packets: None,
        script: None,
        deltas: None,
        socket: None,
        lease_root: None,
        memory: 32,
        timeout: 60,
        hold_ms: 0,
        packet_interval_ms: 0,
        json: false,
        job_id: None,
        ui_root: None,
        content_root: None,
        port: 5467,
        serve_seconds: 0,
    };
    let mut seen = HashSet::new();
    while let Some(flag) = args.next() {
        if !seen.insert(flag.clone()) {
            return Err(format!("duplicate option: {flag}"));
        }
        if flag == "--json" {
            options.json = true;
            continue;
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--snapshot" => options.snapshot = value.into(),
            "--packets" => options.packets = Some(value.into()),
            "--script" => options.script = Some(value.into()),
            "--deltas" => options.deltas = Some(value.into()),
            "--socket" => options.socket = Some(value.into()),
            "--lease-root" => options.lease_root = Some(value.into()),
            "--lua-memory-mib" => {
                options.memory = value.parse().map_err(|_| "invalid Lua memory limit")?
            }
            "--timeout-seconds" => {
                options.timeout = value.parse().map_err(|_| "invalid timeout")?
            }
            "--hold-ms" => options.hold_ms = value.parse().map_err(|_| "invalid hold duration")?,
            "--packet-interval-ms" => {
                options.packet_interval_ms = value.parse().map_err(|_| "invalid packet interval")?
            }
            "--job-id" => options.job_id = Some(value.parse().map_err(|_| "invalid job id")?),
            "--ui-root" => options.ui_root = Some(value.into()),
            "--content-root" => options.content_root = Some(value.into()),
            "--port" => options.port = value.parse().map_err(|_| "invalid port")?,
            "--serve-seconds" => {
                options.serve_seconds = value.parse().map_err(|_| "invalid duration")?
            }
            _ => return Err(format!("unknown option: {flag}")),
        }
    }
    let remote = matches!(options.mode.as_str(), "submit" | "status" | "leave");
    let replay = matches!(
        options.mode.as_str(),
        "replay" | "replay-stream" | "world-replay"
    );
    if !remote && !replay && options.snapshot.as_os_str().is_empty() {
        return Err("--snapshot is required".into());
    }
    if replay != options.packets.is_some() {
        return Err("--packets is required only for replay".into());
    }
    if replay && (!options.snapshot.as_os_str().is_empty() || options.deltas.is_some()) {
        return Err(
            "replay reads its world from --packets, without a snapshot or delta input".into(),
        );
    }
    if !(8..=512).contains(&options.memory) {
        return Err("Lua memory limit must be 8..512 MiB".into());
    }
    if options.hold_ms > 60_000 {
        return Err("--hold-ms must be at most 60000".into());
    }
    if matches!(
        options.mode.as_str(),
        "run" | "replay" | "replay-stream" | "submit"
    ) && options.script.is_none()
    {
        return Err("run, replay and submit require --script".into());
    }
    if options.mode == "console" && (options.socket.is_none() || options.lease_root.is_none()) {
        return Err("console requires --socket and --lease-root".into());
    }
    if !matches!(
        options.mode.as_str(),
        "run" | "replay" | "replay-stream" | "submit" | "serve"
    ) && options.script.is_some()
    {
        return Err("script requires run, replay or submit".into());
    }
    if !matches!(
        options.mode.as_str(),
        "run" | "replay" | "replay-stream" | "world-replay"
    ) && (options.hold_ms != 0 || options.json)
    {
        return Err("hold and json options require run or replay".into());
    }
    if options.packet_interval_ms > 1000
        || (options.mode != "replay-stream" && seen.contains("--packet-interval-ms"))
    {
        return Err("--packet-interval-ms requires replay-stream and must be 0..1000".into());
    }
    if !matches!(options.mode.as_str(), "console" | "serve")
        && !remote
        && (options.socket.is_some() || options.lease_root.is_some())
    {
        return Err("socket and lease options require console".into());
    }
    if remote
        && (options.socket.is_none()
            || !options.snapshot.as_os_str().is_empty()
            || options.lease_root.is_some()
            || options.deltas.is_some())
    {
        return Err(
            "submit/status/leave require only a socket and their operation arguments".into(),
        );
    }
    if (options.mode == "status") != options.job_id.is_some() {
        return Err("--job-id is required only for status".into());
    }
    if options.mode == "serve" && options.socket.is_some() != options.lease_root.is_some() {
        return Err("serve needs both --socket and --lease-root when either is provided".into());
    }
    if options.mode != "serve"
        && ["--ui-root", "--content-root", "--port", "--serve-seconds"]
            .iter()
            .any(|key| seen.contains(*key))
    {
        return Err("UI options require serve mode".into());
    }
    if options.serve_seconds > 86400 {
        return Err("serve duration must be 0..86400 seconds".into());
    }
    Ok(options)
}

fn read_file(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|e| format!("cannot open input: {e}"))?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err(format!(
            "input must be a regular file of at most {limit} bytes"
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err("input grew beyond its size limit".into());
    }
    Ok(bytes)
}

fn read_json(path: &Path) -> Result<Value, String> {
    serde_json::from_slice(&read_file(path, SNAPSHOT_LIMIT)?)
        .map_err(|e| format!("invalid snapshot JSON: {e}"))
}

/// Offline transcript: a little-endian u32 length followed by each packet.
/// Only schema/globals/data replication is accepted; no replies are produced.
fn for_packets(
    path: &Path,
    mut accept: impl FnMut(&[u8], usize) -> Result<(), String>,
) -> Result<usize, String> {
    let file = File::open(path).map_err(|e| format!("cannot open packet transcript: {e}"))?;
    let info = file.metadata().map_err(|e| e.to_string())?;
    if !info.is_file() || info.len() > 512 * 1024 * 1024 {
        return Err("packet transcript must be a regular file at most 512 MiB".into());
    }
    let mut input = BufReader::new(file);
    // Legacy binary transcripts remain supported; native JSON capture replay
    // skips credentials and unrelated traffic without materializing the array.
    if input
        .fill_buf()
        .map_err(|e| e.to_string())?
        .iter()
        .find(|b| !b.is_ascii_whitespace())
        == Some(&b'[')
    {
        return headless_luau::capture_replay::read(input, accept);
    }
    let mut count = 0;
    let mut total_bytes = 0_u64;
    loop {
        let mut header = [0_u8; 4];
        if input.read(&mut header[..1]).map_err(|e| e.to_string())? == 0 {
            break;
        }
        input
            .read_exact(&mut header[1..])
            .map_err(|_| "truncated packet length")?;
        let length = u32::from_le_bytes(header) as usize;
        if !(1..=16 * 1024 * 1024).contains(&length) {
            return Err("transcript packet size must be 1 byte through 16 MiB".into());
        }
        total_bytes += 4 + length as u64;
        if total_bytes > 512 * 1024 * 1024 {
            return Err("packet transcript grew beyond 512 MiB".into());
        }
        let mut payload = vec![0; length];
        input
            .read_exact(&mut payload)
            .map_err(|_| "truncated packet payload")?;
        accept(&payload, count + 1)?;
        count += 1;
    }
    Ok(count)
}

fn committed_sequence(outcome: IngestOutcome) -> Option<u64> {
    match outcome {
        IngestOutcome::GlobalsCommitted {
            packet_sequence, ..
        }
        | IngestOutcome::ReplicationCommitted {
            packet_sequence, ..
        } => Some(packet_sequence),
        IngestOutcome::SchemaInstalled { .. } => None,
    }
}

fn world_replay(options: &Options) -> Result<Value, String> {
    let started = Instant::now();
    let mut session = SessionCore::with_limits(4, 32 * 1024 * 1024).map_err(|e| e.to_string())?;
    let reader = session.retain(0).map_err(|e| e.to_string())?;
    let mut peak_rss = 0;
    let count = for_packets(options.packets.as_deref().unwrap(), |payload, number| {
        if options.timeout != 0 && started.elapsed() >= Duration::from_secs(options.timeout) {
            return Err("world replay exceeded requested duration".into());
        }
        let result = session
            .ingest(payload)
            .map_err(|e| format!("packet {number}: {e}"))?;
        if let Some(sequence) = committed_sequence(result) {
            session
                .acknowledge(reader, sequence)
                .map_err(|e| e.to_string())?;
        }
        peak_rss = peak_rss.max(current_process_rss_bytes().unwrap_or(0));
        Ok(())
    })?;
    let nodes = session.world_view().map(|v| v.node_count()).unwrap_or(0);
    let result = json!({"ok":true,"runtime":"rust_received_world","rust_allocator":RUST_ALLOCATOR_LABEL,"live_game_session":false,
        "network_used":false,"scripts_executed":false,"python_processes":0,
        "replayed_packets":count,"world_nodes":nodes,"world_sequence":session.world_sequence(),
        "rejected_packets":session.rejected_packets(),"elapsed_seconds":started.elapsed().as_secs_f64(),
        "resident_bytes_at_end":current_process_rss_bytes().ok(),"peak_sampled_resident_bytes":peak_rss,
        "peak_kernel_resident_bytes":rove_client_core::memory::peak_process_rss_bytes().ok(),
        "storage":session.storage_profile(),"property_storage":rove_client_core::property_storage::statistics(),
        "lightweight_assets":rove_client_core::lightweight::enabled()});
    thread::sleep(Duration::from_millis(options.hold_ms));
    Ok(result)
}

fn replay_snapshot(path: &Path) -> Result<(Value, usize), String> {
    let mut session = SessionCore::with_limits(4, 32 * 1024 * 1024).map_err(|e| e.to_string())?;
    let reader = session.retain(0).map_err(|e| e.to_string())?;
    let count = for_packets(path, |payload, number| {
        let accepted = session
            .ingest(payload)
            .map_err(|e| format!("packet {number}: {e}"))?;
        if let Some(sequence) = committed_sequence(accepted) {
            session
                .acknowledge(reader, sequence)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    })?;
    let schema = session.schema().ok_or("transcript has no network schema")?;
    let received = session
        .snapshot()
        .ok_or("transcript has no globals/world state")?;
    let snapshot = build_runtime_snapshot(schema, &received, &RuntimeSnapshotMetadata::default())?;
    Ok((snapshot, count))
}

fn for_deltas(
    path: Option<&Path>,
    mut apply: impl FnMut(Value) -> Result<(), String>,
) -> Result<(), String> {
    let Some(path) = path else { return Ok(()) };
    let file = File::open(path).map_err(|e| format!("cannot open delta stream: {e}"))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("delta stream must be a regular file".into());
    }
    let mut reader = BufReader::new(file);
    loop {
        let mut data = Vec::new();
        // Bound each line before allocating, including streams without newline.
        let size = reader
            .by_ref()
            .take(SNAPSHOT_LIMIT as u64 + 1)
            .read_until(b'\n', &mut data)
            .map_err(|e| e.to_string())?;
        if size == 0 {
            break;
        }
        if size > SNAPSHOT_LIMIT {
            return Err("delta exceeds 8 MiB".into());
        }
        if data.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value =
            serde_json::from_slice(&data).map_err(|e| format!("invalid delta JSON: {e}"))?;
        drop(data);
        apply(value)?;
    }
    Ok(())
}

fn emit_output(response: &Value, quiet: bool) -> Result<(), String> {
    if quiet {
        return Ok(());
    }
    if let Some(text) = response.get("output").and_then(Value::as_str) {
        io::stdout()
            .lock()
            .write_all(text.as_bytes())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn run(options: &Options, snapshot: Value) -> Result<Value, String> {
    let started = Instant::now();
    let mut driver = Driver::new(snapshot, options.memory)?;
    for_deltas(options.deltas.as_deref(), |delta| {
        let result = driver.call(update_command(delta, 5))?;
        successful(&result)?;
        emit_output(&result, options.json)
    })?;
    let source = String::from_utf8(read_file(options.script.as_ref().unwrap(), SOURCE_LIMIT)?)
        .map_err(|_| "source must be UTF-8")?;
    let result = driver.call(source_command(source, 1000))?;
    emit_output(&result, options.json)?;
    successful(&result)?;
    finish_job(options, driver, result, started)
}

/// Exercise the incremental session-to-VM path against an owned transcript.
/// Packet spacing is selected locally; transcripts contain no timing claims.
fn replay_stream(options: &Options) -> Result<Value, String> {
    let started = Instant::now();
    let source = String::from_utf8(read_file(options.script.as_ref().unwrap(), SOURCE_LIMIT)?)
        .map_err(|_| "source must be UTF-8")?;
    let mut source = Some(source);
    let mut session = SessionCore::with_limits(4, 32 * 1024 * 1024).map_err(|e| e.to_string())?;
    let reader = session.retain(0).map_err(|e| e.to_string())?;
    let mut driver = None::<Driver>;
    let mut result = None::<Value>;
    let mut acknowledged = 0;
    let mut initial_ready = false;
    let mut normalizer = None::<RuntimeDeltaNormalizer>;
    let mut projection = None::<Arc<ProjectionContext>>;
    let mut compacted_nodes = 0usize;
    let mut updates = 0;
    // These receipts need session-specific consumers in the live client.
    // Count them explicitly; an offline replay sends no acknowledgements,
    // ping replies, or region-readiness claims.
    let mut auxiliary = [0_usize; 5];
    let count = for_packets(options.packets.as_deref().unwrap(), |payload, number| {
        if options.timeout != 0 && started.elapsed() >= Duration::from_secs(options.timeout) {
            return Err("packet replay did not complete before the selected timeout".into());
        }
        let previous_world = session.world_view();
        let accepted = session
            .ingest(payload)
            .map_err(|e| format!("packet {number}: {e}"))?;
        let Some(sequence) = committed_sequence(accepted) else {
            normalizer = Some(RuntimeDeltaNormalizer::new(
                session
                    .schema()
                    .ok_or("schema installation has no schema")?,
            )?);
            return Ok(());
        };
        let records = session
            .read_after(reader, acknowledged, 1)
            .map_err(|e| e.to_string())?;
        let entry = records
            .first()
            .ok_or("accepted packet has no journal receipt")?;
        let record = &entry.value;
        initial_ready |= record.tags.contains(&13);
        for (total, count) in auxiliary.iter_mut().zip([
            record.property_changes.len(),
            record.pings.len(),
            record.regions.len(),
            record.metadata.len(),
            record.tags.iter().filter(|tag| **tag != 13).count(),
        ]) {
            *total += count;
        }
        if let Some(vm) = driver.as_mut() {
            let post_view = session.world_view().ok_or("replay shared world missing")?;
            let mut normalized = normalizer
                .as_mut()
                .ok_or("accepted packet has no normalizer")?
                .normalize_for_shared_view_worker(
                    previous_world
                        .as_ref()
                        .ok_or("incremental packet has no previous world")?,
                    &post_view,
                    record,
                    initial_ready,
                )?;
            // Normalization owns the ordered update. Its old class-lookup
            // root is no longer needed while the VM applies the new view.
            drop(previous_world);
            let view = SharedWorldView::projected(
                post_view,
                projection
                    .as_ref()
                    .ok_or("replay projection missing")?
                    .clone(),
            );
            compacted_nodes += compact_delta_with_candidates(
                &mut normalized.worker_delta,
                &view,
                &normalized.compact_create_ids,
            )?;
            let response =
                vm.call_with_shared_world(update_command(normalized.worker_delta, 10), &view)?;
            successful(&response)?;
            emit_output(&response, options.json)?;
            updates += 1;
        } else {
            drop(previous_world);
            let received = session
                .snapshot()
                .ok_or("transcript has no globals/world state")?;
            let mut snapshot = build_runtime_snapshot(
                session.schema().ok_or("transcript has no network schema")?,
                &received,
                &RuntimeSnapshotMetadata {
                    initial_replication_complete: Some(initial_ready),
                    ..Default::default()
                },
            )?;
            drop(received);
            normalizer
                .as_mut()
                .ok_or("initial snapshot has no normalizer")?
                .bind_snapshot_services(&snapshot)?;
            let context = ProjectionContext::from_normalizer(normalizer.as_ref().unwrap());
            let view = SharedWorldView::projected(
                session
                    .world_view()
                    .ok_or("initial replay shared world missing")?,
                context.clone(),
            );
            compacted_nodes += compact_snapshot(&mut snapshot, &view)?;
            projection = Some(context);
            let mut vm = Driver::new_with_shared_world(snapshot, options.memory, &view)?;
            let execution = vm.call(source_command(
                source.take().ok_or("script already started")?,
                1000,
            ))?;
            successful(&execution)?;
            emit_output(&execution, options.json)?;
            result = Some(execution);
            driver = Some(vm);
        }
        session
            .acknowledge(reader, sequence)
            .map_err(|e| e.to_string())?;
        acknowledged = sequence;
        // All receipt consumers succeeded; release the local journal handle
        // before scheduler polling or a packet-spacing sleep.
        drop(records);
        // Update already advances the VM scheduler. A zero-spacing replay
        // does not need a second native request after every network packet.
        if options.packet_interval_ms != 0 {
            let deadline = Instant::now() + Duration::from_millis(options.packet_interval_ms);
            loop {
                let response = driver.as_mut().unwrap().poll()?;
                successful(&response)?;
                emit_output(&response, options.json)?;
                if Instant::now() >= deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
        if number % 2000 == 0 {
            eprintln!(
                "rust replay · {number} packets · {:.2}s",
                started.elapsed().as_secs_f64()
            );
        }
        Ok(())
    })?;
    session.release(reader).map_err(|e| e.to_string())?;
    let received_nodes = session.world_view().map(|v| v.node_count()).unwrap_or(0);
    let property_storage = rove_client_core::property_storage::statistics();
    // The offline decoder state is no longer needed while waiting for the job.
    drop(session);
    let mut driver = driver.ok_or("transcript has no globals/world state")?;
    let final_world_probe =
        driver.call(source_command("return #game:GetDescendants()".into(), 1000))?;
    successful(&final_world_probe)?;
    let mut completed = finish_job(
        options,
        driver,
        result.ok_or("transcript did not start the script")?,
        started,
    )?;
    completed["replayed_packets"] = json!(count);
    completed["received_world_nodes"] = json!(received_nodes);
    completed["property_storage"] = property_storage;
    completed["lightweight_assets"] = json!(rove_client_core::lightweight::enabled());
    completed["vm_descendants_at_end"] = final_world_probe["values"]
        .get(0)
        .cloned()
        .unwrap_or(Value::Null);
    completed["peak_kernel_resident_bytes"] =
        json!(rove_client_core::memory::peak_process_rss_bytes().ok());
    completed["incremental_updates"] = json!(updates);
    completed["shared_received_world"] = json!(true);
    completed["compacted_received_nodes"] = json!(compacted_nodes);
    completed["snapshot_initializations"] = json!(1);
    completed["packet_interval_ms"] = json!(options.packet_interval_ms);
    completed["unconsumed_auxiliary_receipts"] = json!({
        "property_acknowledgements": auxiliary[0], "pings": auxiliary[1],
        "regions": auxiliary[2], "metadata": auxiliary[3], "other_tags": auxiliary[4]
    });
    Ok(completed)
}

fn finish_job(
    options: &Options,
    mut driver: Driver,
    mut result: Value,
    started: Instant,
) -> Result<Value, String> {
    let job_id = result.get("job_id").and_then(Value::as_u64);
    while result.get("state").and_then(Value::as_str) == Some("running") {
        if options.timeout != 0 && started.elapsed() >= Duration::from_secs(options.timeout) {
            return Err("script did not complete before the selected timeout".into());
        }
        let batch = driver.poll()?;
        successful(&batch)?;
        emit_output(&batch, options.json)?;
        result = driver.status(job_id.ok_or("running VM job has no job id")?)?;
        successful(&result)?;
        if result.get("state").and_then(Value::as_str) == Some("running") {
            thread::sleep(Duration::from_millis(5));
        }
    }

    let hold_until = Instant::now() + Duration::from_millis(options.hold_ms);
    while Instant::now() < hold_until {
        let batch = driver.poll()?;
        successful(&batch)?;
        emit_output(&batch, options.json)?;
        thread::sleep(Duration::from_millis(5));
    }
    let rss = current_process_rss_bytes().ok();
    Ok(
        json!({"ok":true,"values":result.get("values").cloned().unwrap_or_else(||json!([])),
        "elapsed_seconds":started.elapsed().as_secs_f64(),"vm_requests":driver.calls,
        "resident_bytes_at_end":rss,"lua_allocator_cap_bytes":u64::from(options.memory)*1024*1024,
        "actor_vm_count":driver.actor_vms,
        "runtime":"rust_in_process_official_luau","rust_allocator":RUST_ALLOCATOR_LABEL,"live_game_session":false}),
    )
}

#[cfg(any(unix, windows))]
fn console(options: &Options, snapshot: Value) -> Result<Value, String> {
    let _lease = SessionLease::acquire(options.lease_root.as_ref().unwrap())?;
    let mut listener = ScriptConsole::bind(options.socket.as_ref().unwrap(), None)?;
    let mut driver = Driver::new(snapshot, options.memory)?;
    for_deltas(options.deltas.as_deref(), |delta| {
        successful(&driver.call(update_command(delta, 10))?)
    })?;
    let mut closing = None;
    eprintln!(
        "Rust Luau console is ready at {}",
        options.socket.as_ref().unwrap().display()
    );
    loop {
        listener.poll(&mut |mut command: Value| {
            if command.get("op").and_then(Value::as_str) == Some("leave") {
                closing = Some(Instant::now() + Duration::from_millis(200));
                return Ok(json!({"ok":true,"values":[],"closed":true}));
            }
            if closing.is_some() {
                return Err("console is closing".into());
            }
            if command.get("op").and_then(Value::as_str) == Some("status") {
                return driver.status(
                    command
                        .get("job_id")
                        .and_then(Value::as_u64)
                        .ok_or("status needs job_id")?,
                );
            }
            if command.get("source").is_some() && command.get("op").is_none() {
                command["op"] = json!("execute");
            }
            let op = command
                .get("op")
                .and_then(Value::as_str)
                .ok_or("request needs op or source")?;
            if !matches!(op, "execute" | "poll" | "render" | "gui_action") {
                return Err(format!("unsupported local console operation: {op}"));
            }
            driver.call(command)
        })?;
        if closing
            .is_some_and(|deadline| !listener.has_pending_peer() || Instant::now() >= deadline)
        {
            break;
        }
        if closing.is_none() {
            let batch = driver.poll()?;
            successful(&batch)?;
            emit_output(&batch, false)?;
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(json!({"ok":true,"values":[],"closed":true,"requests":listener.requests_handled()}))
}

#[cfg(not(any(unix, windows)))]
fn console(_options: &Options, _snapshot: Value) -> Result<Value, String> {
    Err("The socket console currently requires Unix; use repl or run on this platform".into())
}

fn default_ui_root() -> Option<PathBuf> {
    // Release archives are relocatable. The build-machine source path is only
    // a development fallback, never the sole location checked by an .exe.
    let mut candidates = Vec::new();
    if let Ok(executable) = env::current_exe() {
        if let Some(parent) = executable.parent() {
            candidates.push(parent.join("ui"));
        }
    }
    if let Ok(current) = env::current_dir() {
        candidates.push(current.join("ui"));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ui"));
    candidates
        .into_iter()
        .find(|path| path.join("local-session.html").is_file())
}

fn default_content_root() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let path = PathBuf::from("/Applications/Roblox.app/Contents/Resources/content");
        path.is_dir().then_some(path)
    }
    #[cfg(windows)]
    {
        let versions = PathBuf::from(env::var_os("LOCALAPPDATA")?).join("Roblox/Versions");
        let mut candidates: Vec<_> = fs::read_dir(versions)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("content"))
            .filter(|path| path.is_dir())
            .collect();
        candidates.sort_by_key(|path| fs::metadata(path).and_then(|m| m.modified()).ok());
        candidates.pop()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None
    }
}

#[cfg(any(unix, windows))]
fn serve(options: &Options, snapshot: Value) -> Result<Value, String> {
    let _lease = options
        .lease_root
        .as_ref()
        .map(SessionLease::acquire)
        .transpose()?;
    let mut console = options
        .socket
        .as_ref()
        .map(|path| ScriptConsole::bind(path, None))
        .transpose()?;
    let content = options
        .content_root
        .clone()
        .or_else(|| env::var_os("ROBLOX_CONTENT_DIR").map(PathBuf::from))
        .or_else(default_content_root);
    #[cfg(target_os = "macos")]
    let backend = Some(Arc::new(
        headless_luau::engine_services::local_text::LocalTextBackend::new(
            content.clone().unwrap_or_default(),
        ),
    )
        as Arc<dyn headless_luau::engine_services::TextServiceBackend>);
    #[cfg(not(target_os = "macos"))]
    let backend = None;
    let runtime = Driver::with_text_backend(snapshot, options.memory, backend)?;
    let mut session = SessionUi::new(runtime)?;
    if let Some(content) = content {
        session.set_content_root(content)?;
    }
    for_deltas(options.deltas.as_deref(), |delta| {
        successful(&session.runtime.call(update_command(delta, 10))?)
    })?;
    let assets =
        options.ui_root.clone().or_else(default_ui_root).ok_or(
            "Local UI assets unavailable; put ui beside the executable or use --ui-root PATH",
        )?;
    if !assets.join("local-session.html").is_file() {
        return Err("Local UI assets unavailable; use --ui-root PATH".into());
    }
    let (tx, rx) = mpsc::sync_channel(8);
    let mut server = UiServer::bind(
        UiConfig {
            bind: ([127, 0, 0, 1], options.port).into(),
            assets,
            key: random_ui_key().map_err(|e| e.to_string())?,
        },
        tx,
    )
    .map_err(|e| format!("Cannot start local UI: {e}"))?;
    let url = format!("http://{}/session", server.local_addr());
    println!(
        "{}",
        json!({"event":"ready","url":url,"pid":std::process::id(),
        "runtime":"rust_in_process_official_luau","live_game_session":false})
    );
    io::stdout().flush().map_err(|e| e.to_string())?;
    if let Some(path) = &options.script {
        let source = String::from_utf8(read_file(path, SOURCE_LIMIT)?)
            .map_err(|_| "Source must be UTF-8")?;
        let result = session.runtime.call(source_command(source, 250))?;
        emit_output(&result, false)?;
        successful(&result)?;
    }
    let started = Instant::now();
    let mut last_tick = Instant::now();
    let mut closing_at = None;
    loop {
        // The native handle never leaves this thread. All views address this
        // exact owner and at most eight HTTP commands wait for a turn.
        for _ in 0..8 {
            let Ok(command) = rx.try_recv() else { break };
            let response = session.dispatch(command.operation, command.payload);
            let _ = command.reply.try_send(response);
        }
        if let Some(listener) = &mut console {
            listener.poll(&mut |command| session.dispatch_console(command))?;
        }
        if options.serve_seconds != 0
            && started.elapsed() >= Duration::from_secs(options.serve_seconds)
            && !session.is_closing()
        {
            session.close()?;
        }
        if session.is_closing() {
            let at = closing_at.get_or_insert_with(Instant::now);
            if at.elapsed() >= Duration::from_millis(200) {
                break;
            }
        } else if last_tick.elapsed() >= Duration::from_millis(20) {
            let response = session.tick()?;
            successful(&response)?;
            emit_output(&response, false)?;
            last_tick = Instant::now();
        }
        thread::sleep(Duration::from_millis(5));
    }
    drop(rx); // Disconnect queued HTTP replies before joining its workers.
    server.shutdown();
    let mut result = session.runtime.summary();
    result["url"] = json!(url);
    result["elapsed_seconds"] = json!(started.elapsed().as_secs_f64());
    Ok(result)
}

#[cfg(not(any(unix, windows)))]
fn serve(_options: &Options, _snapshot: Value) -> Result<Value, String> {
    Err("The shared session launcher currently requires Unix".into())
}

fn print_job(response: &Value) -> Result<(), String> {
    emit_output(response, false)?;
    let mut summary = serde_json::Map::new();
    for key in ["ok", "job_id", "state", "values", "error", "timed_out"] {
        if let Some(value) = response.get(key) {
            summary.insert(key.into(), value.clone());
        }
    }
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, &Value::Object(summary)).map_err(|e| e.to_string())?;
    writeln!(stdout).map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())
}

fn repl(options: &Options, snapshot: Value) -> Result<Value, String> {
    let mut driver = Driver::new(snapshot, options.memory)?;
    for_deltas(options.deltas.as_deref(), |delta| {
        successful(&driver.call(update_command(delta, 10))?)
    })?;
    let interactive = io::stdin().is_terminal();
    if interactive {
        println!("Rust Luau console. :paste/:end for multiple lines, :status ID, :quit to leave.");
    }
    let mut terminal = Terminal::start(io::stdin(), interactive).map_err(|e| e.to_string())?;
    let mut submitted = BTreeSet::new();
    let mut input_closed = None;
    let mut failed_jobs = 0_u64;
    loop {
        if let Some(event) = terminal.poll() {
            match event {
                TerminalEvent::Execute(source) => {
                    let result = driver.call(source_command(source, 1000))?;
                    terminal.source_complete();
                    if result.get("state").and_then(Value::as_str) == Some("running") {
                        let id = result
                            .get("job_id")
                            .and_then(Value::as_u64)
                            .ok_or("running VM job has no job id")?;
                        submitted.insert(id);
                    } else if result.get("ok") != Some(&Value::Bool(true)) {
                        failed_jobs += 1;
                    }
                    print_job(&result)?;
                }
                TerminalEvent::Status(id) => {
                    let result = driver
                        .status(id)
                        .unwrap_or_else(|error| json!({"ok":false,"job_id":id,"error":error}));
                    print_job(&result)?;
                }
                TerminalEvent::Quit => break,
                TerminalEvent::Help => println!(
                    "Enter Luau source, :paste then :end for multiple lines, :status ID, or :quit."
                ),
                TerminalEvent::InputClosed => input_closed = Some(Instant::now()),
                TerminalEvent::InputError(error) => eprintln!("Input error: {error}"),
            }
        }
        let batch = driver.poll()?;
        successful(&batch)?;
        emit_output(&batch, false)?;
        // A prior job can also complete during an execute or status request.
        // Consult the shared completion cache rather than only the latest poll.
        for id in submitted.iter().copied().collect::<Vec<_>>() {
            let job = driver.status(id)?;
            if job.get("state").and_then(Value::as_str) == Some("running") {
                continue;
            }
            submitted.remove(&id);
            if job.get("ok") != Some(&Value::Bool(true)) {
                failed_jobs += 1;
            }
            print_job(&job)?;
        }
        if let Some(closed_at) = input_closed {
            if submitted.is_empty() {
                break;
            }
            if options.timeout != 0 && closed_at.elapsed() >= Duration::from_secs(options.timeout) {
                return Err(
                    "input ended but submitted scripts did not finish before the timeout".into(),
                );
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    terminal.shutdown();
    Ok(json!({"ok":true,"closed":true,"failed_jobs":failed_jobs,"live_game_session":false}))
}

fn main_result() -> Result<Value, String> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    if arguments
        .first()
        .is_some_and(|mode| matches!(mode.as_str(), "auth" | "join"))
    {
        return join_cli::run(&arguments);
    }
    let options = parse(arguments.into_iter())?;
    if options.mode == "world-replay" {
        return world_replay(&options);
    }
    if matches!(options.mode.as_str(), "submit" | "status" | "leave") {
        let command = match options.mode.as_str() {
            "submit" => source_command(
                String::from_utf8(read_file(options.script.as_ref().unwrap(), SOURCE_LIMIT)?)
                    .map_err(|_| "source must be UTF-8")?,
                250,
            ),
            "status" => json!({"op":"status","job_id":options.job_id.unwrap()}),
            _ => json!({"op":"leave"}),
        };
        let result = send_request(
            options.socket.as_ref().unwrap(),
            command,
            Duration::from_secs(20),
        )?;
        successful(&result)?;
        return Ok(result);
    }
    if options.mode == "replay-stream" {
        return replay_stream(&options);
    }
    if options.mode == "replay" {
        let (snapshot, count) = replay_snapshot(options.packets.as_deref().unwrap())?;
        let mut result = run(&options, snapshot)?;
        result["replayed_packets"] = json!(count);
        return Ok(result);
    }
    let snapshot = read_json(&options.snapshot)?;
    match options.mode.as_str() {
        "run" => run(&options, snapshot),
        "repl" => repl(&options, snapshot),
        "console" => console(&options, snapshot),
        "serve" => serve(&options, snapshot),
        "snapshot" => {
            let mut world = WorldStore::from_snapshot(&snapshot)?;
            drop(snapshot);
            for_deltas(options.deltas.as_deref(), |delta| world.apply_delta(&delta))?;
            Ok(world.to_snapshot())
        }
        _ => unreachable!(),
    }
}

fn main() {
    let result = main_result();
    let failed = result.as_ref().map_or(true, |value| {
        value.get("ok").and_then(Value::as_bool) == Some(false)
    });
    let value = result.unwrap_or_else(|error| json!({"ok":false,"values":[],"error":error}));
    if serde_json::to_writer(io::stdout().lock(), &value).is_err() {
        std::process::exit(1);
    }
    println!();
    if failed {
        std::process::exit(1);
    }
}

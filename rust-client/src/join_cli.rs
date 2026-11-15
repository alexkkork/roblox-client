//! Explicit CLI flows for account validation and ordinary join allocation.
//!
//! Optional transport probing or Rust live reception uses this allocation's
//! actual credentials. Authored scripts and optional own-character physics run
//! inside the Rust live session; replicated game scripts remain separate work.

use crate::{
    account_auth::AccountSession,
    allocation::SessionError,
    join_coordinator::{
        lookup_target, read_private_share_link, request_until_ready,
        resolve_owned_private_server_file, resolve_private_share_link, CancellationToken,
        JoinOutcome, JoinProgress, ProgressState,
    },
};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Auth {
        credential_file: Option<PathBuf>,
    },
    Join {
        credential_file: Option<PathBuf>,
        place_id: Option<u64>,
        private_link_file: Option<PathBuf>,
        owned_private_server_file: Option<PathBuf>,
        job_id: Option<String>,
        transport_seconds: Option<u64>,
        session_seconds: Option<u64>,
        script: Option<PathBuf>,
        lua_memory_mib: Option<u32>,
        physics: bool,
        lightweight: bool,
    },
}

fn parse(args: &[String]) -> Result<Command, String> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(usage());
    };
    let mut credential_file = None;
    let mut place_id = None;
    let mut private_link_file = None;
    let mut owned_private_server_file = None;
    let mut job_id = None;
    let mut transport_seconds = None;
    let mut session_seconds = None;
    let mut script = None;
    let mut lua_memory_mib = None;
    let mut physics = false;
    let mut lightweight = false;
    let mut seen = std::collections::HashSet::new();
    let mut options = args[1..].iter();
    while let Some(flag) = options.next() {
        if !seen.insert(flag.as_str()) {
            return Err("duplicate option".into());
        }
        if command == "join" && flag == "--physics" {
            physics = true;
            continue;
        }
        if command == "join" && flag == "--lightweight" {
            lightweight = true;
            continue;
        }
        let Some(value) = options.next() else {
            return Err(format!("missing value for {flag}"));
        };
        match (command, flag.as_str()) {
            ("auth" | "join", "--credential-file") => {
                if value.is_empty() {
                    return Err("credential file path is empty".into());
                }
                credential_file = Some(value.into());
            }
            ("join", "--place-id") => {
                let id = value
                    .parse::<u64>()
                    .ok()
                    .filter(|id| (1..=9_007_199_254_740_991).contains(id))
                    .ok_or("invalid place id")?;
                place_id = Some(id);
            }
            ("join", "--private-link-file") => {
                if value.is_empty() {
                    return Err("private link file path is empty".into());
                }
                private_link_file = Some(value.into());
            }
            ("join", "--owned-private-server-file") => {
                if value.is_empty() {
                    return Err("owned server file path is empty".into());
                }
                owned_private_server_file = Some(value.into());
            }
            ("join", "--job-id") => {
                if !canonical_uuid(value) {
                    return Err("invalid public server id".into());
                }
                job_id = Some(value.clone());
            }
            ("join", "--transport-seconds") => {
                transport_seconds = Some(
                    value
                        .parse::<u64>()
                        .ok()
                        .filter(|seconds| (1..=30).contains(seconds))
                        .ok_or("transport probe duration must be 1..30 seconds")?,
                );
            }
            ("join", "--session-seconds") => {
                session_seconds = Some(
                    value
                        .parse::<u64>()
                        .ok()
                        .filter(|seconds| (1..=3600).contains(seconds))
                        .ok_or("session duration must be 1..3600 seconds")?,
                );
            }
            ("join", "--script") => {
                if value.is_empty() {
                    return Err("script file path is empty".into());
                }
                script = Some(value.into());
            }
            ("join", "--lua-memory-mib") => {
                lua_memory_mib = Some(
                    value
                        .parse::<u32>()
                        .ok()
                        .filter(|v| (8..=512).contains(v))
                        .ok_or("Lua memory limit must be 8..512 MiB")?,
                );
            }
            _ => return Err("unknown command or option".into()),
        }
    }
    match command {
        "auth" if place_id.is_none() && private_link_file.is_none() && job_id.is_none() => {
            Ok(Command::Auth { credential_file })
        }
        "join" => {
            if transport_seconds.is_some() && session_seconds.is_some() {
                return Err("choose --transport-seconds or --session-seconds".into());
            }
            if script.is_some() && session_seconds.is_none() {
                return Err("--script requires --session-seconds".into());
            }
            if physics && session_seconds.is_none() {
                return Err("--physics requires --session-seconds".into());
            }
            if lightweight && session_seconds.is_none() {
                return Err("--lightweight requires --session-seconds".into());
            }
            if lua_memory_mib.is_some() && script.is_none() {
                return Err("--lua-memory-mib requires --script".into());
            }
            if usize::from(place_id.is_some())
                + usize::from(private_link_file.is_some())
                + usize::from(owned_private_server_file.is_some())
                != 1
            {
                return Err(
                    "join requires exactly one of --place-id, --private-link-file, or --owned-private-server-file".into(),
                );
            }
            if job_id.is_some() && place_id.is_none() {
                return Err("--job-id requires --place-id".into());
            }
            Ok(Command::Join {
                credential_file,
                place_id,
                private_link_file,
                owned_private_server_file,
                job_id,
                transport_seconds,
                session_seconds,
                script,
                lua_memory_mib,
                physics,
                lightweight,
            })
        }
        _ => Err(usage()),
    }
}

/// Run an already parsed `auth` or `join` command. Parser validation and
/// private-target file validation occur before any network access.
pub fn run(args: &[String]) -> Result<Value, String> {
    match parse(args)? {
        Command::Auth { credential_file } => {
            let mut account = load_account(credential_file.as_ref())?;
            account.validate().map_err(|error| error.to_string())?;
            Ok(json!({
                "ok": true,
                "command": "auth",
                "account_authenticated": true,
                "identity": "verified",
            }))
        }
        Command::Join {
            credential_file,
            place_id,
            private_link_file,
            owned_private_server_file,
            job_id,
            transport_seconds,
            session_seconds,
            script,
            lua_memory_mib,
            physics,
            lightweight,
        } => {
            // This CLI owns a fresh process and configures storage before any
            // world or VM worker is constructed.
            if lightweight {
                std::env::set_var("ROVE_LIGHTWEIGHT","1");
                if std::env::var_os("ROVE_PROPERTY_CACHE_MIB").is_none() {
                    std::env::set_var("ROVE_PROPERTY_CACHE_MIB","1");
                }
            }
            let runtime_config = script
                .as_ref()
                .map(|path| {
                    crate::live_runtime::RuntimeConfig::from_file(
                        path,
                        lua_memory_mib.unwrap_or(32),
                    )
                })
                .transpose()?;
            // Validate local file shape and permissions before the authenticated
            // identity request. The URL remains process-local and is never logged.
            let private_link = private_link_file
                .as_ref()
                .map(|path| read_private_share_link(path).map_err(|error| error.to_string()))
                .transpose()?;
            let mut account = load_account(credential_file.as_ref())?;
            let identity = account.validate().map_err(|error| error.to_string())?;
            let user_id = identity.user_id();
            let (mut session, _identity) = account.into_parts();
            let (target, invite) = if let Some(path) = owned_private_server_file {
                let (target, invite) =
                    resolve_owned_private_server_file(&mut session, path, user_id)
                        .map_err(safe_session_error)?;
                (target, Some(invite))
            } else if let Some(link) = private_link.as_deref() {
                let (target, invite) =
                    resolve_private_share_link(&mut session, link).map_err(safe_session_error)?;
                (target, Some(invite))
            } else {
                (
                    lookup_target(&mut session, place_id, job_id.as_deref(), user_id)
                        .map_err(safe_session_error)?,
                    None,
                )
            };
            let cancellation = CancellationToken::new();
            let mut last_progress: Option<JoinProgress> = None;
            let outcome = request_until_ready(
                &mut session,
                &target,
                invite.as_ref(),
                user_id,
                &cancellation,
                |progress| last_progress = Some(progress),
            )
            .map_err(safe_session_error)?;
            let mut result = match outcome {
                JoinOutcome::Ready {
                    attempts,
                    allocation,
                } => {
                    let mut ready = json!({
                    "ok": true,
                    "command": "join",
                    "account_authenticated": true,
                    "target_verified": true,
                    "allocation_issued": true,
                    "allocation_attempts": attempts,
                    "game_authentication_attempted": false,
                    "player_spawned": false,
                    "stage": "allocation_ready",
                    });
                    if let Some(seconds) = transport_seconds {
                        let probe = crate::connection_probe::probe(&allocation, seconds)?;
                        ready["ok"] = json!(transport_probe_succeeded(&probe));
                        ready["stage"] = probe["stage"].clone();
                        ready["transport"] = probe;
                    }
                    if let Some(seconds) = session_seconds {
                        let runtime_config = runtime_config.map(|config| config.with_animation_backend(
                            std::sync::Arc::new(crate::animation_assets::AnimationAssetBackend::authenticated(
                                crate::engine_services::local_text::default_content_root(), session))));
                        let live = crate::live_session::run_with_options(
                            &allocation,
                            seconds,
                            runtime_config,
                            physics,
                        )?;
                        ready["ok"] = live["ok"].clone();
                        ready["stage"] = live["stage"].clone();
                        ready["game_authentication_attempted"] =
                            live["game_authentication_attempted"].clone();
                        ready["player_spawned"] = live["player_spawned"].clone();
                        ready["live_session"] = live;
                    }
                    ready
                }
                JoinOutcome::PendingExhausted {
                    attempts,
                    http_status,
                    status,
                } => json!({
                    "ok": false,
                    "command": "join",
                    "account_authenticated": true,
                    "allocation_issued": false,
                    "allocation_attempts": attempts,
                    "last_http_status": http_status,
                    "last_allocation_status": status,
                    "stage": "pending_exhausted",
                    "game_authentication_attempted": false,
                    "player_spawned": false,
                }),
                JoinOutcome::Challenge {
                    attempts,
                    http_status,
                } => json!({
                    "ok": false,
                    "command": "join",
                    "account_authenticated": true,
                    "allocation_issued": false,
                    "allocation_attempts": attempts,
                    "last_http_status": http_status,
                    "stage": "account_challenge_required",
                    "game_authentication_attempted": false,
                    "player_spawned": false,
                }),
                JoinOutcome::Rejected {
                    attempts,
                    http_status,
                    status,
                } => json!({
                    "ok": false,
                    "command": "join",
                    "account_authenticated": true,
                    "allocation_issued": false,
                    "allocation_attempts": attempts,
                    "last_http_status": http_status,
                    "last_allocation_status": status,
                    "stage": "allocation_rejected",
                    "game_authentication_attempted": false,
                    "player_spawned": false,
                }),
                JoinOutcome::Cancelled { attempts } => json!({
                    "ok": false,
                    "command": "join",
                    "account_authenticated": true,
                    "allocation_issued": false,
                    "allocation_attempts": attempts,
                    "stage": "cancelled",
                    "game_authentication_attempted": false,
                    "player_spawned": false,
                }),
            };
            if let Some(progress) = last_progress {
                result["last_allocation_attempt"] = json!(progress.attempt);
                result["last_http_status"] = json!(progress.http_status);
                if progress.state == ProgressState::Pending {
                    result["last_allocation_status"] = json!(progress.status);
                }
            }
            result["lightweight_assets"] = json!(rove_client_core::lightweight::enabled());
            Ok(result)
        }
    }
}

fn load_account(path: Option<&PathBuf>) -> Result<AccountSession, String> {
    match path {
        Some(path) => AccountSession::from_private_file(path),
        None => AccountSession::from_default_project(),
    }
    .map_err(|error| error.to_string())
}

fn safe_session_error(error: SessionError) -> String {
    // SessionError contains a static diagnostic, never a response body or URL.
    error.to_string()
}

fn canonical_uuid(value: &str) -> bool {
    let mut nonzero = false;
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                let valid = byte.is_ascii_hexdigit();
                nonzero |= valid && byte != b'0';
                valid
            }
        })
        && nonzero
}

fn transport_probe_succeeded(probe: &Value) -> bool {
    probe["accepted"].as_bool() == Some(true)
        && probe["stayed_open_for_requested_duration"].as_bool() == Some(true)
}

fn usage() -> String {
    "usage: rove auth [--credential-file FILE] | rove join (--place-id ID [--job-id UUID] | --private-link-file FILE | --owned-private-server-file FILE) [--credential-file FILE] [--transport-seconds 1..30 | --session-seconds 1..3600 [--physics] [--lightweight] [--script FILE [--lua-memory-mib 32]]]".into()
}

#[cfg(test)]
mod tests {
    use super::{parse, Command};
    use std::path::PathBuf;

    fn argv(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn lightweight_flag_is_explicit_and_requires_live_session() {
        assert!(parse(&argv(&["join","--place-id","4","--lightweight"])).is_err());
        assert!(matches!(parse(&argv(&["join","--place-id","4","--session-seconds","60","--physics","--lightweight"])),
            Ok(Command::Join {lightweight:true,physics:true,..})));
        assert!(parse(&argv(&["join","--place-id","4","--session-seconds","60","--lightweight","--lightweight"])).is_err());
    }

    #[test]
    fn physics_flag_requires_a_live_session_and_rejects_duplicates() {
        assert!(parse(&argv(&["join", "--place-id", "4", "--physics"])).is_err());
        assert!(matches!(
            parse(&argv(&[
                "join",
                "--physics",
                "--place-id",
                "4",
                "--session-seconds",
                "60"
            ])),
            Ok(Command::Join { physics: true, .. })
        ));
        assert!(matches!(
            parse(&argv(&[
                "join",
                "--place-id",
                "4",
                "--session-seconds",
                "60",
                "--physics"
            ])),
            Ok(Command::Join { physics: true, .. })
        ));
        assert!(parse(&argv(&[
            "join",
            "--place-id",
            "4",
            "--session-seconds",
            "60",
            "--physics",
            "--physics"
        ]))
        .is_err());
        assert!(parse(&argv(&["auth", "--physics"])).is_err());
    }

    #[test]
    fn parses_auth_with_optional_private_credential_path() {
        assert_eq!(
            parse(&argv(&["auth"])).unwrap(),
            Command::Auth {
                credential_file: None
            }
        );
        assert_eq!(
            parse(&argv(&["auth", "--credential-file", "/tmp/cred"])).unwrap(),
            Command::Auth {
                credential_file: Some(PathBuf::from("/tmp/cred"))
            }
        );
    }

    #[test]
    fn requires_exactly_one_explicit_join_target() {
        assert!(parse(&argv(&["join"])).is_err());
        assert!(parse(&argv(&[
            "join",
            "--place-id",
            "4",
            "--private-link-file",
            "x"
        ]))
        .is_err());
        assert!(parse(&argv(&["join", "--place-id", "4"])).is_ok());
        assert!(parse(&argv(&["join", "--private-link-file", "x"])).is_ok());
        assert!(parse(&argv(&["join", "--owned-private-server-file", "x"])).is_ok());
        assert!(parse(&argv(&[
            "join",
            "--owned-private-server-file",
            "x",
            "--private-link-file",
            "y"
        ]))
        .is_err());
        assert!(parse(&argv(&[
            "join",
            "--owned-private-server-file",
            "x",
            "--place-id",
            "4"
        ]))
        .is_err());
    }

    #[test]
    fn rejects_bad_ids_duplicate_flags_and_unexpected_values_without_network() {
        for args in [
            argv(&["join", "--place-id", "0"]),
            argv(&["join", "--place-id", "NaN"]),
            argv(&["join", "--place-id", "4", "--job-id", "bad"]),
            argv(&[
                "join",
                "--place-id",
                "4",
                "--job-id",
                "00000000-0000-0000-0000-000000000000",
            ]),
            argv(&["join", "--place-id", "4", "--place-id", "5"]),
            argv(&["auth", "--credential-file"]),
            argv(&["auth", "--cookie", "secret"]),
            argv(&["join", "--place-id", "4", "--transport-seconds", "0"]),
            argv(&["join", "--place-id", "4", "--transport-seconds", "31"]),
            argv(&["auth", "--transport-seconds", "1"]),
            argv(&["auth", "--session-seconds", "1"]),
            argv(&["join", "--place-id", "4", "--session-seconds", "0"]),
            argv(&["join", "--place-id", "4", "--session-seconds", "3601"]),
            argv(&[
                "join",
                "--place-id",
                "4",
                "--transport-seconds",
                "1",
                "--session-seconds",
                "1",
            ]),
        ] {
            assert!(parse(&args).is_err());
        }
    }

    #[test]
    fn live_receive_requires_an_explicit_duration() {
        let Command::Join {
            session_seconds,
            transport_seconds,
            ..
        } = parse(&argv(&[
            "join",
            "--place-id",
            "4",
            "--session-seconds",
            "30",
        ]))
        .unwrap()
        else {
            panic!("wrong command")
        };
        assert_eq!(session_seconds, Some(30));
        assert_eq!(transport_seconds, None);
        let Command::Join {
            session_seconds, ..
        } = parse(&argv(&["join", "--place-id", "4"])).unwrap()
        else {
            panic!("wrong command")
        };
        assert_eq!(session_seconds, None);
    }

    #[test]
    fn live_script_requires_explicit_session_and_valid_allocator_limit() {
        for options in [
            vec!["--script", "source.luau"],
            vec!["--transport-seconds", "1", "--script", "source.luau"],
            vec!["--session-seconds", "1", "--lua-memory-mib", "32"],
            vec![
                "--session-seconds",
                "1",
                "--script",
                "source.luau",
                "--lua-memory-mib",
                "513",
            ],
        ] {
            let mut args = vec!["join", "--place-id", "4"];
            args.extend(options);
            assert!(parse(&argv(&args)).is_err());
        }
        let Command::Join {
            script,
            lua_memory_mib,
            ..
        } = parse(&argv(&[
            "join",
            "--place-id",
            "4",
            "--session-seconds",
            "1",
            "--script",
            "source.luau",
            "--lua-memory-mib",
            "32",
        ]))
        .unwrap()
        else {
            panic!()
        };
        assert_eq!(script, Some(PathBuf::from("source.luau")));
        assert_eq!(lua_memory_mib, Some(32));
        let error = super::run(&argv(&[
            "join",
            "--place-id",
            "4",
            "--session-seconds",
            "1",
            "--credential-file",
            "/unavailable/credential",
            "--script",
            "/unavailable/source.luau",
        ]))
        .unwrap_err();
        assert_eq!(error, "script file unavailable");
    }

    #[test]
    fn accepts_non_nil_uuid_shape_without_restricting_version_or_variant() {
        assert!(parse(&argv(&[
            "join",
            "--place-id",
            "4",
            "--job-id",
            "ffffffff-ffff-ffff-ffff-ffffffffffff"
        ]))
        .is_ok());
    }

    #[test]
    fn top_level_probe_success_requires_connection_to_last_requested_duration() {
        assert!(super::transport_probe_succeeded(&serde_json::json!({
            "accepted": true,
            "stayed_open_for_requested_duration": true
        })));
        assert!(!super::transport_probe_succeeded(&serde_json::json!({
            "accepted": true,
            "stayed_open_for_requested_duration": false
        })));
        assert!(!super::transport_probe_succeeded(&serde_json::json!({
            "accepted": false,
            "stayed_open_for_requested_duration": true
        })));
    }

    #[test]
    fn validates_private_link_shape_before_any_network_flow() {
        assert!(crate::join_coordinator::valid_share_link_shape(
            "https://www.roblox.com/share?code=0123456789abcdef0123456789abcdef&type=Server"
        ));
        assert!(!crate::join_coordinator::valid_share_link_shape(
            "https://www.roblox.com/share?code=secret&type=Server"
        ));
        assert!(!crate::join_coordinator::valid_share_link_shape(
            "https://example.com/share?code=0123456789abcdef0123456789abcdef&type=Server"
        ));
    }
}

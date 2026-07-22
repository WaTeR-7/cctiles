use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::session::SessionStatus;

type Registry = Arc<Mutex<HashMap<String, Arc<Mutex<SessionStatus>>>>>;

/// Runs a local HTTP server that Claude Code's own hooks mechanism POSTs to,
/// and turns those events into a live `SessionStatus` per tile - replacing
/// the earlier approach of scraping the rendered terminal screen for marker
/// text (see #19's permission-prompt investigation) or inferring state from
/// the `.jsonl` transcript, neither of which could reliably distinguish
/// "working" from "idle", or catch a still-running background task.
///
/// Each spawned `claude` process is launched with `--settings` pointing at
/// [`HookServer::settings_path`], which registers HTTP hooks for the events
/// this cares about. Per docs (and confirmed empirically), hooks supplied
/// via `--settings` are *added* alongside whatever hooks a project already
/// defines for the same event - not a replacement - so this never disturbs
/// a project's own hooks.
pub struct HookServer {
    registry: Registry,
    settings_path: PathBuf,
    /// Only needed by tests, to talk to the server directly instead of
    /// through a real spawned `claude` process.
    #[cfg(test)]
    port: u16,
}

impl HookServer {
    pub fn start() -> anyhow::Result<Self> {
        let server = tiny_http::Server::http("127.0.0.1:0")
            .map_err(|err| anyhow::anyhow!("failed to start hooks HTTP server: {err}"))?;
        let port = match server.server_addr() {
            tiny_http::ListenAddr::IP(addr) => addr.port(),
            other => anyhow::bail!("unexpected hooks server address: {other:?}"),
        };

        let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
        let registry_for_thread = Arc::clone(&registry);
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                handle_request(request, &registry_for_thread);
            }
        });

        let settings_path = write_settings_file(port)?;

        Ok(Self {
            registry,
            settings_path,
            #[cfg(test)]
            port,
        })
    }

    /// Path to the generated hooks settings file, meant to be passed to
    /// `claude --settings <this>` when spawning a session.
    pub fn settings_path(&self) -> &Path {
        &self.settings_path
    }

    /// The port the local hooks server is listening on, baked into
    /// `settings_path`'s contents; exposed mainly for tests that need to
    /// simulate a hook firing without a real `claude` process.
    #[cfg(test)]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Registers a tile's working directory so incoming hook events whose
    /// `cwd` matches it get routed to the returned status handle. Spawning a
    /// new session for the same `dir` re-registers and replaces the old
    /// handle, which is fine - the old one is only reachable from the
    /// `Session` that's being replaced anyway.
    pub fn register(&self, dir: &str) -> Arc<Mutex<SessionStatus>> {
        let state = Arc::new(Mutex::new(SessionStatus::Idle));
        if let Ok(mut map) = self.registry.lock() {
            map.insert(dir.to_string(), Arc::clone(&state));
        }
        state
    }
}

fn handle_request(mut request: tiny_http::Request, registry: &Registry) {
    let mut body = String::new();
    let _ = request.as_reader().read_to_string(&mut body);
    if let Ok(value) = serde_json::from_str::<Value>(&body) {
        apply_event(&value, registry);
    }
    let _ = request.respond(tiny_http::Response::empty(200));
}

/// Updates the registered session (if any) matching the event's `cwd`
/// according to which hook fired. Based on the event semantics confirmed by
/// a live capture during #57's investigation:
/// - `UserPromptSubmit` / `PostToolUse` / a non-`AskUserQuestion` `PreToolUse`
///   mean the session is actively working on its turn.
/// - `PreToolUse` for `AskUserQuestion` means it's now blocked waiting on an
///   interactive answer.
/// - `PermissionRequest` means it's blocked on a permission prompt.
/// - `Stop` means the turn ended - genuinely idle unless its
///   `background_tasks` array (an undocumented but real field) is non-empty,
///   in which case a backgrounded shell is still running unattended.
fn apply_event(value: &Value, registry: &Registry) {
    let Some(cwd) = value.get("cwd").and_then(Value::as_str) else {
        return;
    };
    let Some(event) = value.get("hook_event_name").and_then(Value::as_str) else {
        return;
    };
    let Some(state) = registry.lock().ok().and_then(|map| map.get(cwd).cloned()) else {
        return;
    };
    let Ok(mut status) = state.lock() else {
        return;
    };

    *status = match event {
        "UserPromptSubmit" | "PostToolUse" => SessionStatus::Working,
        "PreToolUse" => {
            if value.get("tool_name").and_then(Value::as_str) == Some("AskUserQuestion") {
                SessionStatus::WaitingForAnswer
            } else {
                SessionStatus::Working
            }
        }
        "PermissionRequest" => SessionStatus::WaitingForPermission,
        "Stop" => {
            let has_background_task = value
                .get("background_tasks")
                .and_then(Value::as_array)
                .is_some_and(|tasks| !tasks.is_empty());
            if has_background_task {
                SessionStatus::BackgroundTaskRunning
            } else {
                SessionStatus::Idle
            }
        }
        _ => return,
    };
}

/// Sends a hook event JSON body to a running `HookServer`, as Claude Code's
/// own HTTP hook delivery would. Exposed for `session.rs`'s tests too, since
/// exercising `Session::status()` end-to-end needs a way to simulate a hook
/// firing without a real `claude` process.
#[cfg(test)]
pub(crate) fn post_hook_event(port: u16, body: &Value) {
    let payload = body.to_string();
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))
        .expect("failed to connect to hook server");
    let request = format!(
        "POST /hook HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
    std::io::Write::write_all(&mut stream, request.as_bytes())
        .expect("failed to send hook request");
}

fn write_settings_file(port: u16) -> anyhow::Result<PathBuf> {
    let url = format!("http://127.0.0.1:{port}/hook");
    let hook_entry =
        |url: &str| json!([{ "matcher": "", "hooks": [{ "type": "http", "url": url }] }]);
    let settings = json!({
        "hooks": {
            "UserPromptSubmit": hook_entry(&url),
            "PreToolUse": hook_entry(&url),
            "PostToolUse": hook_entry(&url),
            "PermissionRequest": hook_entry(&url),
            "Stop": hook_entry(&url),
        }
    });

    let path = std::env::temp_dir().join(format!("cctiles-hooks-{}.json", std::process::id()));
    std::fs::write(&path, serde_json::to_vec(&settings)?)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn status_of(registry: &Registry, dir: &str) -> Option<SessionStatus> {
        registry
            .lock()
            .ok()?
            .get(dir)
            .and_then(|state| state.lock().ok().map(|s| *s))
    }

    fn registry_with(dir: &str, initial: SessionStatus) -> Registry {
        let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
        registry
            .lock()
            .unwrap()
            .insert(dir.to_string(), Arc::new(Mutex::new(initial)));
        registry
    }

    fn event(event_name: &str, cwd: &str, extra: Value) -> Value {
        let mut value = json!({ "hook_event_name": event_name, "cwd": cwd });
        if let (Some(map), Some(extra_map)) = (value.as_object_mut(), extra.as_object()) {
            for (k, v) in extra_map {
                map.insert(k.clone(), v.clone());
            }
        }
        value
    }

    #[test]
    fn user_prompt_submit_sets_working() {
        let registry = registry_with("/proj", SessionStatus::Idle);
        apply_event(&event("UserPromptSubmit", "/proj", json!({})), &registry);
        assert_eq!(status_of(&registry, "/proj"), Some(SessionStatus::Working));
    }

    #[test]
    fn pre_tool_use_for_ask_user_question_sets_waiting_for_answer() {
        let registry = registry_with("/proj", SessionStatus::Working);
        apply_event(
            &event(
                "PreToolUse",
                "/proj",
                json!({"tool_name": "AskUserQuestion"}),
            ),
            &registry,
        );
        assert_eq!(
            status_of(&registry, "/proj"),
            Some(SessionStatus::WaitingForAnswer)
        );
    }

    #[test]
    fn pre_tool_use_for_other_tools_sets_working() {
        let registry = registry_with("/proj", SessionStatus::Idle);
        apply_event(
            &event("PreToolUse", "/proj", json!({"tool_name": "Bash"})),
            &registry,
        );
        assert_eq!(status_of(&registry, "/proj"), Some(SessionStatus::Working));
    }

    #[test]
    fn permission_request_sets_waiting_for_permission() {
        let registry = registry_with("/proj", SessionStatus::Working);
        apply_event(&event("PermissionRequest", "/proj", json!({})), &registry);
        assert_eq!(
            status_of(&registry, "/proj"),
            Some(SessionStatus::WaitingForPermission)
        );
    }

    #[test]
    fn stop_with_background_tasks_sets_background_task_running() {
        let registry = registry_with("/proj", SessionStatus::Working);
        apply_event(
            &event(
                "Stop",
                "/proj",
                json!({"background_tasks": [{"id": "abc", "status": "running"}]}),
            ),
            &registry,
        );
        assert_eq!(
            status_of(&registry, "/proj"),
            Some(SessionStatus::BackgroundTaskRunning)
        );
    }

    #[test]
    fn stop_without_background_tasks_sets_idle() {
        let registry = registry_with("/proj", SessionStatus::Working);
        apply_event(
            &event("Stop", "/proj", json!({"background_tasks": []})),
            &registry,
        );
        assert_eq!(status_of(&registry, "/proj"), Some(SessionStatus::Idle));
    }

    #[test]
    fn stop_missing_background_tasks_field_sets_idle() {
        let registry = registry_with("/proj", SessionStatus::Working);
        apply_event(&event("Stop", "/proj", json!({})), &registry);
        assert_eq!(status_of(&registry, "/proj"), Some(SessionStatus::Idle));
    }

    #[test]
    fn event_for_unregistered_cwd_is_ignored_without_panicking() {
        let registry = registry_with("/proj", SessionStatus::Idle);
        apply_event(
            &event("UserPromptSubmit", "/some/other/dir", json!({})),
            &registry,
        );
        assert_eq!(status_of(&registry, "/proj"), Some(SessionStatus::Idle));
    }

    #[test]
    fn unrecognized_event_name_is_ignored() {
        let registry = registry_with("/proj", SessionStatus::Working);
        apply_event(&event("SessionStart", "/proj", json!({})), &registry);
        assert_eq!(status_of(&registry, "/proj"), Some(SessionStatus::Working));
    }

    #[test]
    fn http_server_delivers_events_end_to_end() {
        let server = HookServer::start().expect("failed to start hook server");
        let dir = "/tmp/cctiles-hooks-e2e-test";
        let state = server.register(dir);
        assert_eq!(*state.lock().unwrap(), SessionStatus::Idle);

        post_hook_event(
            server.port(),
            &json!({"hook_event_name": "UserPromptSubmit", "cwd": dir}),
        );

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut status = *state.lock().unwrap();
        while Instant::now() < deadline && status != SessionStatus::Working {
            std::thread::sleep(Duration::from_millis(20));
            status = *state.lock().unwrap();
        }
        assert_eq!(status, SessionStatus::Working);
    }

    #[test]
    fn settings_file_points_at_the_running_server_port() {
        let server = HookServer::start().expect("failed to start hook server");
        let contents =
            std::fs::read_to_string(server.settings_path()).expect("failed to read settings file");
        assert!(contents.contains(&format!("127.0.0.1:{}", server.port())));
        assert!(contents.contains("PermissionRequest"));
        assert!(contents.contains("Stop"));
    }
}

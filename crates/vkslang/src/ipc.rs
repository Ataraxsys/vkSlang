//! Control socket server for vkslang-ui (one per process).

use crate::config::{self, Source};
use crate::control::control;
use crate::{log_debug, log_info, log_warn};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Once;
use vkslang_ipc::{Request, Response};

/// Starts the listener thread once per process.
pub fn start() {
    static START: Once = Once::new();
    if !config::get().ipc {
        return;
    }
    START.call_once(|| {
        let spawned = std::thread::Builder::new().name("vkslang-ipc".into()).spawn(|| {
            if let Err(e) = serve() {
                log_warn!("control socket unavailable: {e}");
            }
        });
        if let Err(e) = spawned {
            log_warn!("cannot spawn control thread: {e}");
        }
    });
}

fn serve() -> std::io::Result<()> {
    let dir = vkslang_ipc::socket_dir();
    std::fs::create_dir_all(&dir)?;
    let path: PathBuf = vkslang_ipc::socket_path(std::process::id());
    // A leftover from a previous process that had the same pid.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    log_info!("control socket {}", path.display());
    for stream in listener.incoming().flatten() {
        let _ = std::thread::Builder::new()
            .name("vkslang-ipc-client".into())
            .spawn(move || client(stream));
    }
    Ok(())
}

fn client(stream: UnixStream) {
    log_debug!("control client connected");
    let Ok(read_half) = stream.try_clone() else { return };
    let mut writer = stream;
    for line in BufReader::new(read_half).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => handle(request),
            Err(e) => Response::Error { message: format!("bad request: {e}") },
        };
        let Ok(mut out) = serde_json::to_string(&response) else { break };
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() {
            break;
        }
    }
    log_debug!("control client disconnected");
}

pub fn handle(request: Request) -> Response {
    let mut ctl = control();
    match request {
        Request::GetState => {}
        Request::SetParam { name, value } => {
            if !value.is_finite() {
                return Response::Error { message: "value must be finite".into() };
            }
            if let Some(p) = ctl.params.iter_mut().find(|p| p.name == name) {
                p.value = value;
            } else if !ctl.params.is_empty() {
                return Response::Error { message: format!("unknown parameter '{name}'") };
            }
            ctl.overrides.insert(name, value);
            ctl.params_gen += 1;
        }
        Request::ResetParams => {
            ctl.overrides.clear();
            for p in &mut ctl.params {
                p.value = p.initial;
            }
            ctl.params_gen += 1;
        }
        Request::LoadPreset { path } => {
            let path = PathBuf::from(path);
            if !path.is_file() {
                return Response::Error { message: format!("{} does not exist", path.display()) };
            }
            ctl.preset = Some(path);
            ctl.overrides.clear();
            ctl.preset_gen += 1;
            ctl.loading = true;
            ctl.error = None;
        }
        Request::SetEnabled { enabled } => ctl.enabled = enabled,
        Request::SetSource { source } => match Source::from_ipc(&source) {
            Ok(source) => {
                if source != ctl.source {
                    ctl.source = source;
                    ctl.source_gen += 1;
                }
            }
            Err(message) => return Response::Error { message },
        },
    }
    Response::State(ctl.snapshot())
}

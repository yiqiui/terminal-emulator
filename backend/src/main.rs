use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use dbx_plugin_sdk::{PluginEmitter, PluginError, PluginHandler, PluginMetadata, PluginServer, PluginTransport, RequestContext};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use serde_json::{json, Value};

const OUTPUT_CHANNEL: &str = "terminal/output";
const INPUT_CHANNEL: &str = "terminal/input";
const EXIT_CHANNEL: &str = "terminal/exit";
const EXITED_EVENT: &str = "terminal/exited";

/// Wire framing for the two data channels: every payload starts with the
/// little-endian u32 session id, then the data. One channel pair serves all
/// sessions, so the bridge keeps a single subscription.
const SESSION_ID_BYTES: usize = 4;
const BS: &str = "\\";

/// Output coalescing tuned for the JSON/base64 host bridge. A PTY delivers tiny
/// reads (often one byte per echoed keypress) while every bridge frame costs a
/// base64 round trip into the sandboxed UI, so batch by size threshold AND time
/// window — the same trick as nyaterm's SessionOutputCoalescer.
const OUTPUT_FLUSH_BYTES: usize = 64 * 1024;
const OUTPUT_FLUSH_WINDOW_MS: u64 = 8;
const READ_CHUNK: usize = 8 * 1024;

struct Session {
    writer: Mutex<Box<dyn Write + Send>>,
    /// PTY-only: resize handle. None for telnet/serial/raw-TCP transports.
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
    exited: Arc<AtomicBool>,
    exit_code: Arc<Mutex<Option<i64>>>,
    /// Session generation: bumped when the session is stopped, so background
    /// threads belonging to a stopped session stop emitting.
    epoch: Arc<AtomicU64>,
    shell: String,
}

impl Session {
    fn is_running(&self) -> bool {
        !self.exited.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct TerminalPlugin {
    sessions: Mutex<HashMap<u32, Arc<Session>>>,
    next_session_id: AtomicU32,
    connections: Mutex<HashMap<String, SshConnection>>,
}

#[derive(Clone)]
struct SshConnection {
    host: String,
    port: String,
    user: Option<String>,
}

impl PluginHandler for TerminalPlugin {
    fn handle(&self, _context: RequestContext, method: &str, params: Value, emitter: &PluginEmitter) -> Result<Value, PluginError> {
        match method {
            "terminal/start" => self.start(params, emitter),
            "terminal/resize" => self.resize(&params),
            "terminal/stop" => self.stop(&params),
            "terminal/status" => self.status(&params),
            "terminal/shells" => Ok(json!({ "shells": available_shells() })),
            "terminal/serialPorts" => Ok(json!({ "ports": list_serial_ports() })),
            "sftp/list" => sftp_list(&params),
            "system/stats" => system_stats(),
            "sftp/mkdir" => sftp_batch_op(&params, SftpOp::Mkdir),
            "sftp/delete" => sftp_batch_op(&params, SftpOp::Delete),
            "sftp/download" => sftp_download(&params),
            "sftp/upload" => sftp_upload(&params),
            "terminal/connectionInfo" => self.connection_info(&params),
            "connection/test" => Self::test_ssh_connection(&params),
            "connection/connect" => self.connect_connection(&params),
            "connection/disconnect" => self.disconnect_connection(&params),
            _ => Err(PluginError::method_not_found(method)),
        }
    }

    fn handle_binary(&self, channel: &str, data: Vec<u8>, _emitter: &PluginEmitter) -> Result<(), PluginError> {
        match channel {
            "terminal/input" => {
                if data.len() < SESSION_ID_BYTES {
                    return Err(PluginError::new(-32602, "Input frame is missing the session id"));
                }
                let session_id = u32::from_le_bytes(data[..SESSION_ID_BYTES].try_into().unwrap());
                self.write_input(session_id, data[SESSION_ID_BYTES..].to_vec())
            }
            _ => Err(PluginError::new(-32601, format!("Unknown binary channel: {channel}"))),
        }
    }
}

impl TerminalPlugin {
    fn start(&self, params: Value, emitter: &PluginEmitter) -> Result<Value, PluginError> {
        let pty_system = NativePtySystem::default();
        let cols = params.get("cols").and_then(Value::as_u64).unwrap_or(80).clamp(2, 500) as u16;
        let rows = params.get("rows").and_then(Value::as_u64).unwrap_or(24).clamp(2, 300) as u16;
        let shell = params.get("shell").and_then(Value::as_str).map(str::to_string);

        // Telnet / Serial bypass the PTY entirely: a raw duplex stream plays
        // the same reader/writer roles as the ConPTY master handles.
        if shell.as_deref() == Some("telnet") {
            return start_telnet(self, &params, cols, rows, emitter);
        }
        if shell.as_deref() == Some("serial") {
            return start_serial(self, &params, cols, rows, emitter);
        }

        let mut command = match shell.as_deref() {
            Some("ssh") => ssh_command(&params)?,
            other => shell_command(other)?,
        };
        if let Some(cwd) = params.get("cwd").and_then(Value::as_str).filter(|value| !value.is_empty()) {
            command.cwd(cwd);
        }

        let pair = pty_system
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|error| PluginError::new(-32000, format!("Failed to open PTY: {error}")))?;
        let mut child = pair.slave.spawn_command(command).map_err(spawn_error)?;
        drop(pair.slave);

        let session_id = self.next_session_id.fetch_add(1, Ordering::SeqCst);
        let session = Arc::new(Session {
            writer: Mutex::new(pair.master.take_writer().map_err(writer_error)?),
            master: Mutex::new(Some(pair.master)),
            killer: Mutex::new(Some(child.clone_killer())),
            exited: Arc::new(AtomicBool::new(false)),
            exit_code: Arc::new(Mutex::new(None)),
            epoch: Arc::new(AtomicU64::new(0)),
            shell: shell_name(shell.as_deref()).to_string(),
        });
        self.sessions.lock().map_err(lock_error)?.insert(session_id, session.clone());

        let mut reader = session
            .master
            .lock()
            .map_err(lock_error)?
            .as_ref()
            .expect("session just created")
            .try_clone_reader()
            .map_err(read_error)?;
        let emitter = emitter.clone();

        let (chunk_tx, chunk_rx) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            let mut buffer = [0u8; READ_CHUNK];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(size) => {
                        if chunk_tx.send(buffer[..size].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let coalescer_emitter = emitter.clone();
        let coalescer_epoch = session.epoch.clone();
        thread::spawn(move || {
            enum Outcome {
                Chunk(Vec<u8>),
                Timeout,
                Closed,
            }
            let mut pending: Vec<u8> = Vec::with_capacity(OUTPUT_FLUSH_BYTES);
            let mut window_opened: Option<Instant> = None;
            let mut emit = move |payload: &[u8]| -> bool {
                let mut frame = Vec::with_capacity(SESSION_ID_BYTES + payload.len());
                frame.extend_from_slice(&session_id.to_le_bytes());
                frame.extend_from_slice(payload);
                coalescer_emitter.binary(OUTPUT_CHANNEL, &frame).is_ok()
            };
            loop {
                if coalescer_epoch.load(Ordering::SeqCst) != 0 {
                    break;
                }
                let window_expired = window_opened.is_some_and(|started| started.elapsed() >= Duration::from_millis(OUTPUT_FLUSH_WINDOW_MS));
                let outcome = if window_expired || pending.len() >= OUTPUT_FLUSH_BYTES {
                    match chunk_rx.try_recv() {
                        Ok(chunk) => Outcome::Chunk(chunk),
                        Err(mpsc::TryRecvError::Empty) => Outcome::Timeout,
                        Err(mpsc::TryRecvError::Disconnected) => Outcome::Closed,
                    }
                } else if window_opened.is_some() {
                    match chunk_rx.recv_timeout(Duration::from_millis(OUTPUT_FLUSH_WINDOW_MS)) {
                        Ok(chunk) => Outcome::Chunk(chunk),
                        Err(mpsc::RecvTimeoutError::Timeout) => Outcome::Timeout,
                        Err(mpsc::RecvTimeoutError::Disconnected) => Outcome::Closed,
                    }
                } else {
                    match chunk_rx.recv() {
                        Ok(chunk) => Outcome::Chunk(chunk),
                        Err(_) => Outcome::Closed,
                    }
                };
                match outcome {
                    Outcome::Chunk(chunk) => {
                        if window_opened.is_none() {
                            window_opened = Some(Instant::now());
                        }
                        pending.extend_from_slice(&chunk);
                        if pending.len() < OUTPUT_FLUSH_BYTES && !window_expired {
                            continue;
                        }
                    }
                    Outcome::Timeout => {}
                    Outcome::Closed if pending.is_empty() => break,
                    Outcome::Closed => {}
                }
                if !pending.is_empty() && !emit(&pending) {
                    break;
                }
                pending.clear();
                window_opened = None;
            }
        });

        let exit_emitter = emitter.clone();
        let exit_session = session.clone();
        thread::spawn(move || {
            let status = child.wait();
            // Epoch != 0 means the session was stopped by request; that exit is
            // known to the caller and must not reach the UI.
            if exit_session.epoch.load(Ordering::SeqCst) != 0 {
                return;
            }
            let code = status.ok().map(|status| status.exit_code() as i64).unwrap_or(-1);
            if let Ok(mut slot) = exit_session.exit_code.lock() {
                *slot = Some(code);
            }
            exit_session.exited.store(true, Ordering::SeqCst);
            let mut frame = format!("{code}").into_bytes();
            let _ = exit_emitter.binary(EXIT_CHANNEL, &{
                let mut framed = session_id.to_le_bytes().to_vec();
                framed.append(&mut frame);
                framed
            });
            let _ = exit_emitter.event(EXITED_EVENT, json!({ "sessionId": session_id, "code": code }));
        });

        Ok(json!({
            "success": true,
            "sessionId": session_id,
            "shell": session.shell,
            "cols": cols,
            "rows": rows,
        }))
    }

    fn session(&self, session_id: u32) -> Result<Arc<Session>, PluginError> {
        self.sessions
            .lock()
            .map_err(lock_error)?
            .get(&session_id)
            .cloned()
            .ok_or_else(|| PluginError::new(-32010, format!("Unknown session: {session_id}")))
    }

    fn write_input(&self, session_id: u32, data: Vec<u8>) -> Result<(), PluginError> {
        let session = self.session(session_id)?;
        if !session.is_running() {
            return Err(PluginError::new(-32010, format!("Session {session_id} is not running")));
        }
        let mut writer = session.writer.lock().map_err(lock_error)?;
        writer.write_all(&data).map_err(write_error).and_then(|_| writer.flush().map_err(write_error))
    }

    fn resize(&self, params: &Value) -> Result<Value, PluginError> {
        let session_id = params.get("sessionId").and_then(Value::as_u64).unwrap_or(0) as u32;
        let session = self.session(session_id)?;
        let cols = params.get("cols").and_then(Value::as_u64).unwrap_or(80).clamp(2, 500) as u16;
        let rows = params.get("rows").and_then(Value::as_u64).unwrap_or(24).clamp(2, 300) as u16;
        let guard = session.master.lock().map_err(lock_error)?;
        if let Some(master) = guard.as_ref() {
            master
                .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
                .map_err(|error| PluginError::new(-32000, format!("Failed to resize PTY: {error}")))?;
        }
        Ok(json!({ "success": true, "sessionId": session_id, "cols": cols, "rows": rows }))
    }

    fn stop(&self, params: &Value) -> Result<Value, PluginError> {
        let session_id = params.get("sessionId").and_then(Value::as_u64).unwrap_or(0) as u32;
        let session = self.session(session_id)?;
        // Invalidate first: a stop-requested exit never reaches the UI.
        session.epoch.fetch_add(1, Ordering::SeqCst);
        {
            let mut writer = session.writer.lock().map_err(lock_error)?;
            let _ = writer.write_all(b"exit\r");
            let _ = writer.flush();
        }
        if let Ok(mut killer) = session.killer.lock() {
            if let Some(mut killer) = killer.take() {
                let _ = killer.kill();
            }
        }
        if let Ok(mut master_slot) = session.master.lock() {
            *master_slot = None;
        }
        self.sessions.lock().map_err(lock_error)?.remove(&session_id);
        Ok(json!({ "success": true, "sessionId": session_id }))
    }

    fn status(&self, params: &Value) -> Result<Value, PluginError> {
        let session_id = params.get("sessionId").and_then(Value::as_u64).unwrap_or(0) as u32;
        let sessions = self.sessions.lock().map_err(lock_error)?;
        if let Some(session) = sessions.get(&session_id) {
            Ok(json!({
                "running": session.is_running(),
                "exited": session.exited.load(Ordering::SeqCst),
                "exitCode": *session.exit_code.lock().map_err(lock_error)?,
                "shell": session.shell,
            }))
        } else {
            Ok(json!({ "running": false, "exited": false, "exitCode": Option::<i64>::None }))
        }
    }

    fn connection_info(&self, params: &Value) -> Result<Value, PluginError> {
        let connection_id = params.get("connectionId").and_then(Value::as_str).unwrap_or("");
        let found = self
            .connections
            .lock()
            .map_err(lock_error)?
            .get(connection_id)
            .map(|connection| {
                json!({
                    "found": true,
                    "host": connection.host,
                    "port": connection.port,
                    "user": connection.user,
                })
            })
            .unwrap_or_else(|| json!({ "found": false }));
        Ok(found)
    }

    fn test_ssh_connection(params: &Value) -> Result<Value, PluginError> {
        let connection = params.get("connection").cloned().unwrap_or_default();
        let host = connection.get("host").and_then(Value::as_str).unwrap_or("").to_string();
        let port: u16 = connection.get("port").and_then(Value::as_u64).unwrap_or(22) as u16;
        if host.is_empty() {
            return Err(PluginError::new(-32602, "Host is required"));
        }
        let timeout = Duration::from_secs(3);
        let address = std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), port))
            .map_err(|error| PluginError::new(-32000, format!("DNS resolution failed for {host}:{port}: {error}")))?
            .next()
            .ok_or_else(|| PluginError::new(-32000, format!("No address resolved for {host}:{port}")))?;
        match std::net::TcpStream::connect_timeout(&address, timeout) {
            Ok(_) => Ok(json!({ "ok": true, "message": format!("TCP {host}:{port} reachable") })),
            Err(error) => Err(PluginError::new(-32000, format!("Cannot reach {host}:{port}: {error}"))),
        }
    }

    fn connect_connection(&self, params: &Value) -> Result<Value, PluginError> {
        let connection = params.get("connection").cloned().unwrap_or_default();
        let connection_id = connection
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| PluginError::new(-32602, "Missing connection id"))?;
        let host = connection.get("host").and_then(Value::as_str).unwrap_or("").to_string();
        if host.is_empty() {
            return Err(PluginError::new(-32602, "SSH connection is missing a host"));
        }
        let port = match connection.get("port").and_then(Value::as_u64) {
            Some(port) => port.to_string(),
            None => connection
                .get("port")
                .and_then(Value::as_str)
                .unwrap_or("22")
                .to_string(),
        };
        let user = connection
            .get("user")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|user| !user.is_empty())
            .map(str::to_string)
            .or_else(|| {
                connection
                    .get("config")
                    .and_then(|config| config.get("user"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        self.connections
            .lock()
            .map_err(lock_error)?
            .insert(connection_id.clone(), SshConnection { host, port, user });
        Ok(json!({ "connected": true, "connectionId": connection_id }))
    }

    fn disconnect_connection(&self, params: &Value) -> Result<Value, PluginError> {
        if let Some(connection_id) = params.get("connection").and_then(|connection| connection.get("id")).and_then(Value::as_str) {
            self.connections.lock().map_err(lock_error)?.remove(connection_id);
        }
        Ok(json!({ "disconnected": true }))
    }
}


fn start_telnet(plugin: &TerminalPlugin, params: &Value, cols: u16, rows: u16, emitter: &PluginEmitter) -> Result<Value, PluginError> {
    use std::net::TcpStream;
    let host = params.get("host").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let port = params.get("port").and_then(Value::as_u64).unwrap_or(23).clamp(1, 65535).to_string();
    if host.is_empty() {
        return Err(PluginError::new(-32602, "Telnet host is required"));
    }
    let stream = TcpStream::connect((host.as_str(), port.parse::<u16>().unwrap_or(23)))
        .map_err(|error| PluginError::new(-32000, format!("Telnet connect {host}:{port} failed: {error}")))?;
    stream.set_nodelay(true).ok();
    let reader = stream.try_clone().map_err(|error| PluginError::new(-32000, format!("Telnet stream clone failed: {error}")))?;
    reader.set_read_timeout(Some(Duration::from_millis(250))).ok();
    let session_id = plugin.next_session_id.fetch_add(1, Ordering::SeqCst);
    let session = Arc::new(Session {
        writer: Mutex::new(Box::new(stream)),
        master: Mutex::new(None),
        killer: Mutex::new(None),
        exited: Arc::new(AtomicBool::new(false)),
        exit_code: Arc::new(Mutex::new(None)),
        epoch: Arc::new(AtomicU64::new(0)),
        shell: format!("telnet {host}:{port}"),
    });
    plugin.sessions.lock().map_err(lock_error)?.insert(session_id, session.clone());
    let emitter = emitter.clone();
    thread::spawn(move || {
        let mut socket = reader;
        let mut pending: Vec<u8> = Vec::with_capacity(OUTPUT_FLUSH_BYTES);
        let mut window_opened: Option<Instant> = None;
        let mut buffer = [0u8; READ_CHUNK];
        loop {
            if session.epoch.load(Ordering::SeqCst) != 0 { break; }
            match socket.read(&mut buffer) {
                Ok(0) => break,
                Ok(size) => {
                    // Strip TELNET IAC negotiation (0xFF ...); text flows as-is.
                    let mut i = 0;
                    while i < size {
                        if buffer[i] == 0xFF {
                            i += match buffer.get(i + 1) {
                                Some(0xFB..=0xFE) => 3,
                                Some(0xFA) => {
                                    let mut j = i + 2;
                                    while j < size && buffer[j] != 0xF0 { j += 1; }
                                    (j + 1 - i).min(size - i)
                                }
                                _ => 2,
                            };
                        } else {
                            let start = i;
                            while i < size && buffer[i] != 0xFF { i += 1; }
                            pending.extend_from_slice(&buffer[start..i]);
                        }
                    }
                    if window_opened.is_none() { window_opened = Some(Instant::now()); }
                    if pending.len() < OUTPUT_FLUSH_BYTES && window_opened.map(|s| s.elapsed() < Duration::from_millis(OUTPUT_FLUSH_WINDOW_MS)).unwrap_or(false) { continue; }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock || error.kind() == std::io::ErrorKind::TimedOut => {
                    if pending.is_empty() { continue; }
                }
                Err(_) => break,
            }
            if !pending.is_empty() {
                let mut frame = session_id.to_le_bytes().to_vec();
                frame.extend_from_slice(&pending);
                if emitter.binary(OUTPUT_CHANNEL, &frame).is_err() { break; }
                pending.clear();
                window_opened = None;
            }
        }
        session.exited.store(true, Ordering::SeqCst);
        if let Ok(mut slot) = session.exit_code.lock() {
            *slot = Some(0);
        }
    });
    Ok(json!({ "success": true, "sessionId": session_id, "shell": format!("telnet {host}:{port}"), "cols": cols, "rows": rows }))
}

fn start_serial(plugin: &TerminalPlugin, params: &Value, cols: u16, rows: u16, emitter: &PluginEmitter) -> Result<Value, PluginError> {
    let path = params.get("serialPath").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let baud = params.get("baud").and_then(Value::as_u64).unwrap_or(115200) as u32;
    if path.is_empty() {
        return Err(PluginError::new(-32602, "Serial port path is required"));
    }
    let port = serialport::new(&path, baud).timeout(Duration::from_millis(120)).open()
        .map_err(|error| PluginError::new(-32000, format!("Serial open {path}@{baud} failed: {error}")))?;
    let session_id = plugin.next_session_id.fetch_add(1, Ordering::SeqCst);
    let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
    let session = Arc::new(Session {
        writer: Mutex::new(Box::new(InputPipe(input_tx))),
        master: Mutex::new(None),
        killer: Mutex::new(None),
        exited: Arc::new(AtomicBool::new(false)),
        exit_code: Arc::new(Mutex::new(None)),
        epoch: Arc::new(AtomicU64::new(0)),
        shell: format!("serial {path}@{baud}"),
    });
    plugin.sessions.lock().map_err(lock_error)?.insert(session_id, session.clone());
    let emitter = emitter.clone();
    thread::spawn(move || {
        let mut port = port;
        let mut pending: Vec<u8> = Vec::with_capacity(OUTPUT_FLUSH_BYTES);
        let mut window_opened: Option<Instant> = None;
        let mut buffer = [0u8; READ_CHUNK];
        loop {
            if session.epoch.load(Ordering::SeqCst) != 0 { break; }
            match port.read(&mut buffer) {
                Ok(0) => {}
                Ok(size) => {
                    pending.extend_from_slice(&buffer[..size]);
                    if window_opened.is_none() { window_opened = Some(Instant::now()); }
                    if pending.len() < OUTPUT_FLUSH_BYTES && window_opened.map(|s| s.elapsed() < Duration::from_millis(OUTPUT_FLUSH_WINDOW_MS)).unwrap_or(false) { continue; }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock || error.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => break,
            }
            if !pending.is_empty() {
                let mut frame = session_id.to_le_bytes().to_vec();
                frame.extend_from_slice(&pending);
                if emitter.binary(OUTPUT_CHANNEL, &frame).is_err() { break; }
                pending.clear();
                window_opened = None;
            }
            while let Ok(chunk) = input_rx.try_recv() {
                if port.write_all(&chunk).is_err() { break; }
            }
        }
        session.exited.store(true, Ordering::SeqCst);
        if let Ok(mut slot) = session.exit_code.lock() {
            *slot = Some(0);
        }
    });
    Ok(json!({ "success": true, "sessionId": session_id, "shell": format!("serial {path}@{baud}"), "cols": cols, "rows": rows }))
}

/// mpsc-backed Write half so session input reaches the serial worker thread.
struct InputPipe(mpsc::Sender<Vec<u8>>);
impl Write for InputPipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.send(buf.to_vec()).map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "serial worker gone"))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

fn list_serial_ports() -> Vec<Value> {
    match serialport::available_ports() {
        Ok(ports) => ports.into_iter().filter_map(|info| match info.port_type {
            serialport::SerialPortType::UsbPort(usb) => Some(json!({ "path": info.port_name, "desc": usb.product.unwrap_or_else(|| "USB serial".into()) })),
            _ => Some(json!({ "path": info.port_name, "desc": "" })),
        }).collect(),
        Err(_) => vec![],
    }
}


static SYSTEM: std::sync::Mutex<Option<sysinfo::System>> = std::sync::Mutex::new(None);

fn system_stats() -> Result<Value, PluginError> {
    let mut guard = SYSTEM.lock().map_err(|_| PluginError::new(-32000, "system monitor poisoned".to_string()))?;
    let sys = guard.get_or_insert_with(sysinfo::System::new);
    sys.refresh_memory();
    sys.refresh_cpu_usage();
    thread::sleep(Duration::from_millis(200));
    sys.refresh_cpu_usage();
    let cpu = sys.global_cpu_usage();
    let mem_total = sys.total_memory() as f64;
    let mem_used = sys.used_memory() as f64;
    let mut disks_total = 0u64;
    let mut disks_avail = 0u64;
    let disks = sysinfo::Disks::new_with_refreshed_list();
    for disk in disks.list() {
        if disk.mount_point().to_string_lossy().len() <= 3 || cfg!(unix) {
            disks_total += disk.total_space();
            disks_avail += disk.available_space();
        }
    }
    let networks = sysinfo::Networks::new_with_refreshed_list();
    let mut rx = 0u64;
    let mut tx = 0u64;
    for (_name, data) in networks.iter() {
        rx += data.total_received();
        tx += data.total_transmitted();
    }
    let host_name = sysinfo::System::host_name().unwrap_or_default();
    let os_version = sysinfo::System::long_os_version().unwrap_or_default();
    let uptime = sysinfo::System::uptime();
    let per_core: Vec<i64> = sys.cpus().iter().map(|cpu| cpu.cpu_usage().round() as i64).collect();
    Ok(json!({
        "cpu": cpu.round() as i64,
        "hostName": host_name,
        "osVersion": os_version,
        "uptime": uptime,
        "perCore": per_core,
        "memUsed": mem_used,
        "memTotal": mem_total,
        "diskTotal": disks_total,
        "diskAvail": disks_avail,
        "netRx": rx,
        "netTx": tx,
    }))
}

/// SFTP via the system sftp.exe client (key / agent auth).
fn locate_sftp() -> Option<String> {
    if cfg!(windows) {
        let bs = BS;
        let mut candidates = vec![format!("C:{bs}Windows{bs}System32{bs}OpenSSH{bs}sftp.exe")];
        if let Ok(windir) = std::env::var("SystemRoot") {
            candidates.insert(0, format!("{windir}{bs}System32{bs}OpenSSH{bs}sftp.exe"));
        }
        candidates.push("sftp.exe".to_string());
        resolve_first(candidates)
    } else {
        resolve_first(vec!["sftp".to_string()])
    }
}

fn sftp_target(params: &Value) -> Result<String, PluginError> {
    let host = params.get("host").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if host.is_empty() { return Err(PluginError::new(-32602, "SFTP host is required")); }
    let user = params.get("user").and_then(Value::as_str).map(str::trim).filter(|u| !u.is_empty());
    Ok(match user { Some(u) => format!("{u}@{host}"), None => host })
}

fn run_sftp_batch(params: &Value, commands: &str) -> Result<String, PluginError> {
    let sftp = locate_sftp().ok_or_else(|| PluginError::new(-32602, "sftp.exe not found on this machine".to_string()))?;
    let target = sftp_target(params)?;
    let port = params.get("port").and_then(Value::as_u64).unwrap_or(22).to_string();
    let batch_path = std::env::temp_dir().join(format!("dbx-sftp-batch-{}.txt", std::process::id()));
    std::fs::write(&batch_path, commands).map_err(|error| PluginError::new(-32000, format!("batch write failed: {error}")))?;
    let output = std::process::Command::new(&sftp)
        .args(["-oBatchMode=yes", "-oStrictHostKeyChecking=accept-new", "-P", &port, "-b"])
        .arg(&batch_path)
        .arg(&target)
        .output()
        .map_err(|error| PluginError::new(-32000, format!("sftp exec failed: {error}")))?;
    let _ = std::fs::remove_file(&batch_path);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(PluginError::new(-32000, format!("sftp failed (key/agent auth required): {} {}", stdout.trim(), stderr.trim())));
    }
    Ok(stdout)
}

fn sftp_list(params: &Value) -> Result<Value, PluginError> {
    let remote = params.get("remotePath").and_then(Value::as_str).unwrap_or(".");
    let raw = run_sftp_batch(params, &format!("ls -la {remote}\n"))?;
    let mut entries = vec![];
    for line in raw.lines() {
        if !line.starts_with('-') && !line.starts_with('d') { continue; }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 9 { continue; }
        let name = fields[8..].join(" ");
        if name == "." || name == ".." { continue; }
        entries.push(json!({
            "name": name,
            "dir": line.starts_with('d'),
            "size": fields[4].parse::<u64>().unwrap_or(0),
        }));
    }
    Ok(json!({ "entries": entries }))
}

enum SftpOp { Mkdir, Delete }
fn sftp_batch_op(params: &Value, op: SftpOp) -> Result<Value, PluginError> {
    let remote = params.get("remotePath").and_then(Value::as_str).unwrap_or("");
    if remote.is_empty() { return Err(PluginError::new(-32602, "remotePath is required")); }
    let command = match op {
        SftpOp::Mkdir => format!("mkdir {remote}\n"),
        SftpOp::Delete => format!("rm {remote}\n"),
    };
    run_sftp_batch(params, &command)?;
    Ok(json!({ "success": true }))
}

fn sftp_download(params: &Value) -> Result<Value, PluginError> {
    let remote = params.get("remotePath").and_then(Value::as_str).unwrap_or("");
    if remote.is_empty() { return Err(PluginError::new(-32602, "remotePath is required")); }
    let name = remote.rsplit('/').next().unwrap_or("download.bin").to_string();
    let temp = std::env::temp_dir().join(format!("dbx-sftp-dl-{name}"));
    let _ = std::fs::remove_file(&temp);
    run_sftp_batch(params, &(format!("get {remote} {}", temp.display()) + "\n"))?;
    let bytes = std::fs::read(&temp).map_err(|error| PluginError::new(-32000, format!("temp read failed: {error}")))?;
    let _ = std::fs::remove_file(&temp);
    if bytes.len() > 6 * 1024 * 1024 {
        return Err(PluginError::new(-32000, "File exceeds the 6 MB inline transfer limit; use sz or the official SFTP plugin".to_string()));
    }
    Ok(json!({ "name": name, "size": bytes.len(), "dataBase64": base64::encode(&bytes) }))
}

fn sftp_upload(params: &Value) -> Result<Value, PluginError> {
    let remote = params.get("remotePath").and_then(Value::as_str).unwrap_or("");
    let data = params.get("dataBase64").and_then(Value::as_str).unwrap_or("");
    if remote.is_empty() || data.is_empty() { return Err(PluginError::new(-32602, "remotePath and dataBase64 are required")); }
    let bytes = base64::decode(data).map_err(|error| PluginError::new(-32000, format!("bad base64: {error}")))?;
    if bytes.len() > 6 * 1024 * 1024 {
        return Err(PluginError::new(-32000, "File exceeds the 6 MB inline transfer limit".to_string()));
    }
    let temp = std::env::temp_dir().join(format!("dbx-sftp-ul-{}", std::process::id()));
    std::fs::write(&temp, &bytes).map_err(|error| PluginError::new(-32000, format!("temp write failed: {error}")))?;
    let result = run_sftp_batch(params, &(format!("put {} {remote}", temp.display()) + "\n"));
    let _ = std::fs::remove_file(&temp);
    result?;
    Ok(json!({ "success": true, "size": bytes.len() }))
}
fn shell_command(shell: Option<&str>) -> Result<CommandBuilder, PluginError> {
    match shell {
        Some("powershell") => Ok(CommandBuilder::new("powershell.exe")),
        Some("pwsh") => locate_pwsh()
            .map(CommandBuilder::new)
            .ok_or_else(|| PluginError::new(-32602, "pwsh.exe not found; install PowerShell 7 first".to_string())),
        Some("cmd") => Ok(CommandBuilder::new("cmd.exe")),
        Some("bash") => Ok(bash_command()),
        Some("ssh") => locate_ssh()
            .map(CommandBuilder::new)
            .ok_or_else(|| PluginError::new(-32602, "ssh.exe not found on this machine".to_string())),
        Some(other) => Err(PluginError::new(-32602, format!("Unsupported shell: {other}"))),
        None => {
            if cfg!(windows) {
                Ok(CommandBuilder::new("powershell.exe"))
            } else {
                Ok(CommandBuilder::new("bash"))
            }
        }
    }
}

/// ssh user@host session driven by the system OpenSSH client: password,
/// key and agent prompts all happen interactively inside the PTY.
fn ssh_command(params: &Value) -> Result<CommandBuilder, PluginError> {
    let host = params.get("host").and_then(Value::as_str).map(str::trim).filter(|h| !h.is_empty())
        .ok_or_else(|| PluginError::new(-32602, "SSH target host is required".to_string()))?;
    let user = params.get("user").and_then(Value::as_str).map(str::trim).filter(|u| !u.is_empty());
    let port = params.get("port").and_then(Value::as_u64).unwrap_or(22).clamp(1, 65535).to_string();
    let target = match user {
        Some(user) => format!("{user}@{host}"),
        None => host.to_string(),
    };
    let mut command = locate_ssh()
        .map(CommandBuilder::new)
        .ok_or_else(|| PluginError::new(-32602, "ssh.exe not found on this machine".to_string()))?;
    command.args(["-p", &port, "-o", "StrictHostKeyChecking=accept-new", &target]);
    Ok(command)
}

/// On Windows, bare "bash" resolves from PATH to System32\bash.exe — the WSL
/// launcher — which exits with code 1 inside a ConPTY when no default distro
/// answers. Prefer an explicit Git Bash / MSYS2 install; a PATH hit is tried
/// last because the WSL launcher also lurks there. Always run interactively.
fn bash_command() -> CommandBuilder {
    let mut command = if cfg!(windows) {
        let mut candidates = windows_bash_candidates();
        candidates.push("bash.exe".to_string());
        let picked = resolve_first(candidates).unwrap_or_else(|| "bash".to_string());
        CommandBuilder::new(picked)
    } else {
        CommandBuilder::new("bash")
    };
    command.args(["-i"]);
    command
}

fn windows_bash_candidates() -> Vec<String> {
    let mut candidates = Vec::new();
    for (variable, suffix) in [
        ("ProgramFiles", r"Git\bin\bash.exe"),
        ("ProgramFiles(x86)", r"Git\bin\bash.exe"),
        ("LOCALAPPDATA", r"Programs\Git\bin\bash.exe"),
        ("ProgramFiles", r"Git\usr\bin\bash.exe"),
    ] {
        if let Ok(base) = std::env::var(variable) {
            candidates.push(format!("{base}\\{suffix}"));
        }
    }
    candidates
}

fn bash_available() -> bool {
    if !cfg!(windows) {
        return true;
    }
    let mut candidates = windows_bash_candidates();
    candidates.push("bash.exe".to_string());
    resolve_first(candidates).is_some()
}

fn locate_pwsh() -> Option<String> {
    if cfg!(windows) {
        let mut candidates = vec![];
        if let Ok(base) = std::env::var("ProgramFiles") {
            for version in ["7", "6"] {
                candidates.push(format!("{base}\\PowerShell\\{version}\\pwsh.exe"));
            }
        }
        candidates.push("pwsh.exe".to_string());
        resolve_first(candidates)
    } else {
        resolve_first(vec!["pwsh".to_string()])
    }
}

fn locate_ssh() -> Option<String> {
    if cfg!(windows) {
        let mut candidates = vec![r"C:\Windows\System32\OpenSSH\ssh.exe".to_string()];
        if let Ok(windir) = std::env::var("SystemRoot") {
            candidates.insert(0, format!("{windir}\\System32\\OpenSSH\\ssh.exe"));
        }
        candidates.push("ssh.exe".to_string());
        resolve_first(candidates)
    } else {
        resolve_first(vec!["ssh".to_string()])
    }
}

/// Try absolute candidates first, then fall back to a PATH lookup.
fn resolve_first(candidates: Vec<String>) -> Option<String> {
    for candidate in &candidates {
        if std::path::Path::new(candidate).is_file() {
            return Some(candidate.clone());
        }
    }
    let last = candidates.last()?;
    if std::path::Path::new(last).is_file() {
        return Some(last.clone());
    }
    let path = std::env::var("PATH").ok()?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(last))
        .find(|full| full.is_file())
        .map(|full| full.to_string_lossy().into_owned())
}

fn available_shells() -> Vec<Value> {
    let mut shells = vec![
        json!({ "id": "powershell", "available": cfg!(windows) }),
        json!({ "id": "pwsh", "available": locate_pwsh().is_some() }),
        json!({ "id": "cmd", "available": cfg!(windows) }),
        json!({ "id": "bash", "available": bash_available() }),
        json!({ "id": "ssh", "available": locate_ssh().is_some() }),
    ];
    if !cfg!(windows) {
        shells.retain(|shell| shell["id"] != Value::from("cmd") && shell["id"] != Value::from("powershell"));
    }
    shells
}

fn shell_name(shell: Option<&str>) -> &str {
    shell.unwrap_or(if cfg!(windows) { "powershell" } else { "bash" })
}

fn lock_error(_: impl std::error::Error) -> PluginError {
    PluginError::new(-32000, "Terminal session registry is poisoned")
}
fn spawn_error(error: anyhow::Error) -> PluginError { PluginError::new(-32000, format!("Failed to start shell: {error}")) }
fn read_error(error: anyhow::Error) -> PluginError { PluginError::new(-32000, format!("Failed to clone PTY reader: {error}")) }
fn writer_error(error: anyhow::Error) -> PluginError { PluginError::new(-32000, format!("Failed to open PTY writer: {error}")) }
fn write_error(error: std::io::Error) -> PluginError { PluginError::new(-32000, format!("Failed to write terminal input: {error}")) }

fn main() -> std::io::Result<()> {
    PluginServer::new(
        PluginMetadata::new("com.local.terminal-emulator", env!("CARGO_PKG_VERSION")).with_capability("binary"),
        TerminalPlugin::default(),
    )
    .transport(PluginTransport::Framed)
    .serve()
}

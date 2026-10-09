use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;

/// Handles one request line, returning `Ok(())` for success or an error message
/// that is sent back to the client.
type Handler = dyn Fn(String) -> Result<(), String> + Send + Sync + 'static;

/// Path of the Unix domain socket used to talk to the running bar server.
pub fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("gpui-shell.sock");
        }
    }
    PathBuf::from("/tmp/gpui-shell.sock")
}

/// Connects to a running server, sends `line`, and returns its response.
pub fn send_command(line: &str) -> Result<String, String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|err| {
        format!(
            "cannot reach a running instance at {}: {err}",
            path.display()
        )
    })?;

    stream
        .write_all(line.as_bytes())
        .and_then(|()| stream.write_all(b"\n"))
        .map_err(|err| format!("failed to send command: {err}"))?;

    let mut response = String::new();
    BufReader::new(&stream)
        .read_line(&mut response)
        .map_err(|err| format!("failed to read response: {err}"))?;

    Ok(response)
}

/// Binds the command socket and serves connections on a background thread,
/// forwarding each request line to `handler`.
///
/// Exits the process if another live server already owns the socket; a stale
/// socket left by a crash is replaced.
pub fn serve(handler: impl Fn(String) -> Result<(), String> + Send + Sync + 'static) {
    let path = socket_path();

    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(&path).is_ok() {
                eprintln!("gpui-shell: already running (socket: {})", path.display());
                std::process::exit(1);
            }
            let _ = std::fs::remove_file(&path);
            match UnixListener::bind(&path) {
                Ok(listener) => listener,
                Err(err) => {
                    eprintln!("gpui-shell: failed to bind {}: {err}", path.display());
                    std::process::exit(1);
                }
            }
        }
        Err(err) => {
            eprintln!("gpui-shell: failed to bind {}: {err}", path.display());
            std::process::exit(1);
        }
    };

    let handler: Arc<Handler> = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let handler = handler.clone();
                    std::thread::spawn(move || handle_client(stream, handler));
                }
                Err(err) => eprintln!("gpui-shell: accept failed: {err}"),
            }
        }
    });
}

fn handle_client(mut stream: UnixStream, handler: Arc<Handler>) {
    let mut line = String::new();

    let response = {
        let mut reader = BufReader::new(&stream);
        match reader.read_line(&mut line) {
            Ok(0) => "error: empty request\n".to_string(),
            Ok(_) => match handler(line.trim().to_string()) {
                Ok(()) => "ok\n".to_string(),
                Err(err) => format!("error: {err}\n"),
            },
            Err(err) => format!("error: {err}\n"),
        }
    };

    let _ = stream.write_all(response.as_bytes());
}

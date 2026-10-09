use crate::ipc;

/// Sends a request line to the running server and exits with its status.
pub fn run(line: String) -> ! {
    match ipc::send_command(&line) {
        Ok(response) => {
            print!("{response}");
            let ok = response.starts_with("ok");
            std::process::exit(if ok { 0 } else { 1 });
        }
        Err(err) => {
            eprintln!("gpui-shell: {err}");
            eprintln!(
                "gpui-shell: start the server first by running `gpui-shell` without a command."
            );
            std::process::exit(1);
        }
    }
}

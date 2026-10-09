//! Private local discovery and a bounded GUI message queue.
use super::{session_id, Envelope, Reply};
use iced::futures::{channel::mpsc, Stream};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

pub(in crate::app) fn subscribe() -> iced::Subscription<Envelope> {
    // SecurePlan CAD runs no GUI-control listener (DSK-02).
    if cfg!(feature = "secureplan") {
        return iced::Subscription::none();
    }
    iced::Subscription::run(worker)
}

fn worker() -> impl Stream<Item = Envelope> {
    iced::stream::channel(32, |sender| async move {
        std::thread::spawn(move || {
            if let Err(error) = listen(sender) {
                eprintln!("Automation unavailable: {error}");
            }
        });
        iced::futures::future::pending::<()>().await;
    })
}

fn listen(sender: mpsc::Sender<Envelope>) -> std::io::Result<()> {
    // SecurePlan CAD runs no GUI-control listener and writes no token file.
    if cfg!(feature = "secureplan") {
        return Err(std::io::Error::other(
            "The automation listener is disabled in SecurePlan CAD.",
        ));
    }
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let dir = crate::config::config_dir()
        .ok_or_else(|| std::io::Error::other("No user directory"))?
        .join("automation");
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).map_err(std::io::Error::other)?;
    let token: String = secret.iter().map(|v| format!("{v:02x}")).collect();
    let descriptor = json!({"protocol":1,"session_id":session_id(),"pid":std::process::id(),"port":listener.local_addr()?.port(),"token":token,"executable":std::env::current_exe()?.to_string_lossy()});
    let path = dir.join(format!("{}.json", session_id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    write!(file, "{descriptor}")?;
    drop(file);
    let _ = crate::mcp::sync_agent_tool_schemas();
    let clients = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming().flatten() {
        if clients.fetch_add(1, Ordering::SeqCst) >= 8 {
            clients.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let clients = clients.clone();
        let mut sender = sender.clone();
        let token = token.clone();
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
            if let Ok(write_half) = stream.try_clone() {
                let mut reader = BufReader::new(stream);
                let mut writer = write_half;
                // One bounded request per connection; operation polling reconnects.
                let mut line = String::new();
                if reader.by_ref().take(1_048_577).read_line(&mut line).is_ok()
                    && line.len() <= 1_048_576
                    && line.ends_with('\n')
                {
                    if let Ok(mut request) = serde_json::from_str::<Value>(&line) {
                        if request["token"].as_str() == Some(token.as_str()) {
                            if let Some(object) = request.as_object_mut() {
                                object.remove("token");
                            }
                            let (reply, response) = std::sync::mpsc::channel();
                            let result = if sender
                                .try_send(Envelope {
                                    request,
                                    reply: Reply::Native(reply),
                                })
                                .is_ok()
                            {
                                response.recv_timeout(Duration::from_secs(15)).unwrap_or_else(|_| json!({"ok":false,"code":"response_timeout","error":"Query operation status with the same request_id; execution may continue."}))
                            } else {
                                json!({"ok":false,"code":"busy","error":"GUI queue full"})
                            };
                            let _ = writeln!(writer, "{result}");
                        }
                    }
                }
            }
            clients.fetch_sub(1, Ordering::SeqCst);
        });
    }
    let _ = std::fs::remove_file(path);
    Ok(())
}

#[cfg(all(test, feature = "secureplan"))]
mod secureplan_tests {
    use super::*;

    #[test]
    fn no_listener_and_no_token_file() {
        let descriptor = crate::config::config_dir()
            .map(|dir| dir.join("automation").join(format!("{}.json", session_id())));
        let (sender, _receiver) = mpsc::channel(1);
        assert!(listen(sender).is_err(), "the GUI-control listener started");
        assert!(
            descriptor.is_none_or(|path| !path.exists()),
            "a GUI-control token file was written"
        );
    }
}

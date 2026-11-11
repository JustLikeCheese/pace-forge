//! Web 控制台：登录、记录列表、轨迹地图。
//!
//! 设计上刻意保持薄：
//! - 会话状态只存一份 `Client`（含 uid/token/unid），进程退出即失效
//! - 不落盘密码，登录成功后密码直接丢弃
//! - 所有业务逻辑复用 `api/*`，Web 层只做转发与 JSON 序列化
//!
//! 轨迹字段问题：服务端记录详情里字段名不稳定（不同版本、不同接口各有差异），
//! 所以后端不解析轨迹，原样返回 `raw`，由前端递归探测坐标数组。这样即使
//! 服务端改字段名，地图仍然能画出来。

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tiny_http::{Header, Method, Response, Server};

use crate::api::client::{Client, DeviceProfile};
use crate::api::login::{login, LoginOutcome, UserProfile};
use crate::api::records;

type Shared = Arc<Mutex<State>>;

struct State {
    /// 账号库（落盘）。
    book: crate::api::accounts::Book,
    /// 每个账号一个独立 Client，token 互不干扰。
    clients: std::collections::HashMap<String, Client>,
    /// 当前选中的账号 id。
    current: Option<String>,
}

impl State {
    #[allow(dead_code)]
    fn new() -> Self {
        Self {
            book: crate::api::accounts::Book::default(),
            clients: std::collections::HashMap::new(),
            current: None,
        }
    }

    /// 从磁盘恢复所有账号的登录态。
    fn restore() -> Self {
        let book = crate::api::accounts::load();
        let mut clients = std::collections::HashMap::new();
        for a in &book.accounts {
            if a.logged_in() {
                clients.insert(a.id.clone(), make_client(&a.uid, &a.token, &a.unid));
            }
        }
        let current = book
            .accounts
            .iter()
            .find(|a| a.logged_in())
            .map(|a| a.id.clone());
        if let Some(id) = &current {
            println!("[web] 恢复 {} 个账号，当前={id}", clients.len());
        }
        Self { book, clients, current }
    }

    /// 当前账号。
    fn current_account(&self) -> Option<&crate::api::accounts::Account> {
        self.current.as_ref().and_then(|id| self.book.get(id))
    }

    /// 当前账号是否可发请求。
    fn logged_in(&self) -> bool {
        self.current_account().map(|a| a.logged_in()).unwrap_or(false)
    }

    /// 当前账号的 Client。
    fn client(&self) -> Option<&Client> {
        self.current.as_ref().and_then(|id| self.clients.get(id))
    }

    /// 某账号的 Client。
    fn client_of(&self, id: &str) -> Option<&Client> {
        self.clients.get(id)
    }

    /// 用新登录结果更新一个账号。
    fn apply_login(&mut self, id: &str, p: &UserProfile) {
        if let Some(a) = self.book.get_mut(id) {
            a.uid = p.uid.clone();
            a.token = p.token.clone();
            a.unid = p.unid.clone();
            a.name = p.name.clone();
            a.school = p.campus_name.clone();
            a.last_login_ms = crate::crypto::envelope::now_ms() as i64;
            // 能登录成功就说明凭据有效，标记启用；否则 login-all 会一直跳过它
            a.enabled = true;
        }
        self.clients
            .insert(id.to_string(), make_client(&p.uid, &p.token, &p.unid));
        self.current = Some(id.to_string());
        let _ = crate::api::accounts::save(&self.book);
        // 同步写一份 session.json，让 CLI 也能直接用（原来只有 Web 有登录态，
        // CLI 永远报「未登录」）。
        let sess = crate::api::session::Session {
            uid: p.uid.clone(),
            token: p.token.clone(),
            unid: p.unid.clone(),
            name: p.name.clone(),
            campus_name: p.campus_name.clone(),
            depart: p.depart.clone(),
            grade_class: p.grade_class.clone(),
            ..Default::default()
        };
        let _ = crate::api::session::save_session(&sess);
    }

    fn persist(&self) {
        if let Err(e) = crate::api::accounts::save(&self.book) {
            eprintln!("保存账号库失败: {e}");
        }
    }
}

/// 对某个账号发一次服务端退出，失败也返回 false 而不是报错。
///
/// 优先用内存里的 client；没有（进程刚重启、或者这个账号还没被激活过）
/// 就用存盘的 uid/token 临时建一个，只为发这一个请求。
///
/// 这一步不能省。重启后直接删账号时内存里没有 client，如果就此跳过，
/// 服务端会话仍然活着，账号在服务端还是「已登录」——下次登录 10121 并
/// 锁一小时。用户视角就是「我明明删了，怎么还锁我」。
fn remote_logout(state: &Shared, id: &str) -> bool {
    let mut tmp: Option<Client> = None;
    {
        let mut st = match state.lock() {
            Ok(s) => s,
            Err(_) => return false,
        };
        if let Some(c) = st.clients.get_mut(id) {
            return crate::api::login::logout(c).unwrap_or(false);
        }
        if let Some(a) = st.book.get(id) {
            if a.logged_in() {
                tmp = Some(make_client(&a.uid, &a.token, &a.unid));
            }
        }
    }
    match tmp.as_mut() {
        Some(c) => crate::api::login::logout(c).unwrap_or(false),
        None => false,
    }
}

fn make_client(uid: &str, token: &str, unid: &str) -> Client {
    // 每个账号独立的 device_id，避免被按设备聚合。
    //
    // 作用域必须和登录时完全一致：登录前不知道 uid，用的是手机号。
    // 之前这里传 uid、登录时传手机号，同一个账号会算出两套 device_id，
    // 服务端当成两台手机，直接触发 10121「此账号已在其他设备登录」并把
    // 账号锁一小时。
    let mut c = Client::new(DeviceProfile::for_account(&device_scope(uid)));
    c.creds = crate::api::client::Credentials {
        uid: Some(uid.to_string()),
        token: Some(token.to_string()),
        unid: Some(unid.to_string()),
    };
    c
}

/// 设备作用域：一个账号有且只有一套 device_id。
///
/// 账号库里 id 是 uid、username 是手机号。登录前后拿到的标识不同，但必须
/// 映射到同一个作用域，否则就是「换手机登录」。
fn device_scope(id_or_username: &str) -> String {
    let book = crate::api::accounts::load();
    // id 命中就直接用 username，保证和登录时一致
    if let Some(a) = book.accounts.iter().find(|a| a.id == id_or_username) {
        if !a.username.is_empty() {
            return a.username.clone();
        }
    }
    // 传进来本来就是手机号
    id_or_username.to_string()
}

/// 启动 Web 控制台。
/// 控制台口令。设置环境变量 `PACE_FORGE_TOKEN` 后，所有 `/api/*` 请求
/// 必须带 `X-Pace-Token` 或 `?token=`，否则 401。
///
/// 不设置时只允许本机访问——绑 0.0.0.0 又没有口令，等于把「登录/提交/删账号」
/// 暴露给同网段任意主机，密码还是明文 HTTP 传的。
fn auth_token() -> Option<String> {
    std::env::var("PACE_FORGE_TOKEN").ok().filter(|t| !t.is_empty())
}

/// 请求来源是否本机。
fn is_loopback(remote: &str) -> bool {
    let r = remote.trim();
    r.starts_with("127.")
        || r.starts_with("::1")
        || r.starts_with("[::1]")
        || r == "::ffff:127.0.0.1"
}

/// 鉴权通过返回 Ok，否则返回 Err(要回给客户端的话)。
fn check_auth(request: &tiny_http::Request, path: &str, query: &str) -> Result<(), String> {
    // 静态页不拦，否则连页面都打不开
    if path == "/" || path == "/index.html" {
        return Ok(());
    }
    match auth_token() {
        Some(want) => {
            let from_header = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("X-Pace-Token"))
                .map(|h| h.value.as_str().to_string());
            let from_query = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("token="))
                .map(url_decode);
            let got = from_header.or(from_query).unwrap_or_default();
            if got == want {
                Ok(())
            } else {
                Err("缺少或错误的口令（X-Pace-Token）".into())
            }
        }
        None => {
            let remote = request.remote_addr().map(|a| a.ip().to_string()).unwrap_or_default();
            if is_loopback(&remote) {
                Ok(())
            } else {
                Err(format!(
                    "拒绝来自 {remote} 的访问：控制台默认只允许本机，\
                     远程使用请设置 PACE_FORGE_TOKEN 并在请求头带 X-Pace-Token"
                ))
            }
        }
    }
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| -> Option<u8> {
                    match c {
                        b'0'..=b'9' => Some(c - b'0'),
                        b'a'..=b'f' => Some(c - b'a' + 10),
                        b'A'..=b'F' => Some(c - b'A' + 10),
                        _ => None,
                    }
                };
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(a), Some(c)) => {
                        out.push(a * 16 + c);
                        i += 3;
                    }
                    _ => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

pub fn serve(addr: &str) -> Result<(), String> {
    let server = Server::http(addr).map_err(|e| format!("监听 {addr} 失败: {e}"))?;
    let state: Shared = Arc::new(Mutex::new(State::restore()));

    println!("Web 控制台已启动: http://{addr}");
    match auth_token() {
        Some(_) => println!("已启用口令（PACE_FORGE_TOKEN），远程访问需在请求头带 X-Pace-Token"),
        None => println!(
            "未设置 PACE_FORGE_TOKEN：只接受本机访问。\
             要让同网段设备访问，请先 `export PACE_FORGE_TOKEN=你的口令`"
        ),
    }
    println!("按 Ctrl+C 停止");

    for request in server.incoming_requests() {
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            // 单个请求里的 panic 只杀这一个线程，不带走整个进程和正在跑的批次。
            // 之前 release 配的是 panic="abort"，一处 unwrap 失败就等于全崩。
            let guard = std::panic::AssertUnwindSafe(|| handle(request, state));
            match std::panic::catch_unwind(guard) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("请求处理失败: {e}"),
                Err(_) => eprintln!("请求处理 panic（已隔离，进程继续）"),
            }
        });
    }
    Ok(())
}

fn handle(mut request: tiny_http::Request, state: Shared) -> Result<(), String> {
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("/").to_string();
    let query = url.split('?').nth(1).unwrap_or("").to_string();
    let method = request.method().clone();

    if path.starts_with("/api/") {
        if let Err(msg) = check_auth(&request, &path, &query) {
            return json_err(request, &msg);
        }
    }

    match (method.clone(), path.as_str()) {
        (Method::Get, "/") => respond_html(request, INDEX_HTML),
        (Method::Post, "/api/login") => {
            let body = read_json(&mut request)?;
            handle_login(request, state, &body)
        }
        (Method::Get, "/api/me") => {
            let st = state.lock().map_err(|e| e.to_string())?;
            match st.current_account() {
                Some(a) => json_ok(request, json!({
                    "id": a.id, "uid": a.uid, "unid": a.unid,
                    "name": a.name, "username": a.username, "school": a.school,
                    "loggedIn": a.logged_in(),
                })),
                None => json_err(request, "未登录"),
            }
        }
        (Method::Post, "/api/logout") => {
            // 先通知服务端让 token 失效，再清本地。
            //
            // 顺序不能反：只清本地的话服务端会话还活着，账号被标记为已登录，
            // 再登录会拿到 10121（已在其他设备登录），而且锁一小时。
            let remote = {
                let mut st = state.lock().map_err(|e| e.to_string())?;
                let id = st.current.clone();
                match id.and_then(|i| st.clients.get_mut(&i)) {
                    Some(c) => crate::api::login::logout(c).unwrap_or(false),
                    None => false,
                }
            };
            {
                let mut st = state.lock().map_err(|e| e.to_string())?;
                if let Some(id) = st.current.clone() {
                    if let Some(a) = st.book.get_mut(&id) {
                        a.token.clear();
                        a.uid.clear();
                    }
                    st.clients.remove(&id);
                    st.current = None;
                }
                st.persist();
            }
            crate::api::session::clear_session();
            json_ok(request, json!({"ok": true, "remoteLogout": remote}))
        }
        (Method::Post, "/api/plan") => {
            let body = read_json(&mut request)?;
            handle_plan(request, state, &body)
        }
        (Method::Get, "/api/accounts") => handle_accounts(request, state),
        (Method::Get, "/api/starttimes") => handle_starttimes(request),
        (Method::Post, "/api/accounts") => {
            let body = read_json(&mut request)?;
            handle_add_account(request, state, &body)
        }
        (Method::Post, "/api/accounts/login-all") => handle_login_all(request, state),
        (Method::Post, "/api/batch") => {
            let body = read_json(&mut request)?;
            handle_batch(request, state, &body)
        }
        (Method::Get, "/api/semester") => handle_semester(request, state),
        (Method::Post, "/api/run") => {
            let body = read_json(&mut request)?;
            handle_run(request, state, &body)
        }
        (Method::Get, "/api/fences") => handle_fences(request, state, &query),
        (Method::Get, "/api/records") => handle_records(request, state, &query),
        (Method::Get, "/api/blockers") => handle_blockers_get(request),
        (Method::Post, "/api/blockers") => {
            let body = read_json(&mut request)?;
            handle_blockers_put(request, &body)
        }
        (Method::Post, "/api/obs/fetch") => {
            let body = read_json(&mut request)?;
            handle_obs_fetch(request, state, &body)
        }
        (Method::Post, "/api/raw") => {
            let body = read_json(&mut request)?;
            handle_raw(request, state, &body)
        }
        (Method::Get, "/api/health") => json_ok(request, json!({"ok": true, "version": crate::VERSION})),
        _ => {
            if let Some(rest) = path.strip_prefix("/api/accounts/") {
                // /api/accounts/{id}/switch  |  /api/accounts/{id}  (DELETE)
                let mut it = rest.splitn(2, '/');
                let id = it.next().unwrap_or("").to_string();
                let action = it.next().unwrap_or("");
                let force = query_param(&query, "force")
                    .map(|v| v == "1" || v == "true")
                    .unwrap_or(false);
                return match (method, action) {
                    (Method::Delete, "") => handle_del_account(request, state, &id, force),
                    (Method::Post, "switch") => handle_switch(request, state, &id),
                    (Method::Post, "logout") => handle_logout_one(request, state, &id),
                    (Method::Post, "login") => handle_login_one(request, state, &id),
                    (Method::Post, "toggle") => handle_toggle(request, state, &id),
                    _ => json_err_code(request, 404, "unknown account action"),
                };
            }
            if let Some(rest) = path.strip_prefix("/api/record/") {
                let rrid: i64 = rest.parse().map_err(|_| "rrid 不是数字")?;
                return handle_record_detail(request, state, rrid);
            }
            json_err_code(request, 404, "not found")
        }
    }
}

fn handle_login(request: tiny_http::Request, state: Shared, body: &Value) -> Result<(), String> {
    let username = body.get("username").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let password = body.get("password").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if username.is_empty() || password.is_empty() {
        return json_err(request, "手机号和密码都要填");
    }

    let mut client = Client::new(DeviceProfile::default());
    match login(&mut client, &username, &password) {
        Ok(LoginOutcome::Ok(profile)) => {
            let mut st = state.lock().map_err(|e| e.to_string())?;
            let id = if profile.uid.is_empty() {
                username.clone()
            } else {
                profile.uid.clone()
            };
            // 密码加密存盘，下次不用再输
            let enc = crate::api::accounts::seal_password(&password).unwrap_or_default();
            let mut acc = crate::api::accounts::Account {
                id: id.clone(),
                username: username.clone(),
                password_enc: enc,
                ..Default::default()
            };
            if let Some(old) = st.book.get(&id) {
                acc.enabled = old.enabled;
                acc.last_submit_ms = old.last_submit_ms;
            }
            st.book.upsert(acc);
            st.apply_login(&id, &profile);
            json_ok(request, json!(profile))
        }
        Ok(LoginOutcome::NeedCaptcha { uuid, message }) => {
            json_err(request, &format!("{message}（uuid={uuid}）"))
        }
        Err(e) => json_err(request, &e),
    }
}

/// 轨迹预演：本地生成轨迹 + 提交报文，不联网。
///
/// 这一步能验证轨迹几何、分桶、签名是否合规，但**不会**真的提交——
/// 真实提交需要先拿服务端的规则/围栏/打卡点，且要过静默期。
fn handle_plan(
    request: tiny_http::Request,
    state: Shared,
    body: &serde_json::Value,
) -> Result<(), String> {
    let distance_km = body["distance"].as_f64().unwrap_or(2.0);
    let duration_min = body["duration"].as_f64().unwrap_or(12.0);
    // 0 或缺失都表示「没指定」，交给模型按配速推算。
    // 手填步数很容易和配速对不上——12 分钟 2300 步 = 192 步/分，
    // 而 2.78 m/s 的典型步频只有 158，差 34 步会被自检判 GAIT_MISMATCH。
    let steps = body["steps"]
        .as_i64()
        .filter(|v| *v > 0)
        .map(|v| v as i32);
    let sport_type = body["sportType"].as_i64().unwrap_or(1) as i32;
    let mut lat = body["lat"].as_f64().unwrap_or(0.0);
    let mut lng = body["lng"].as_f64().unwrap_or(0.0);
    let run_mode = body["runMode"].as_i64().unwrap_or(1);
    let start_at_ms = body["startAtMs"].as_i64().filter(|v| *v > 0);

    // 没传坐标就用围栏中心，别用写死的成都坐标
    if lat == 0.0 || lng == 0.0 {
        let st = state.lock().map_err(|e| e.to_string())?;
        if let Some(client) = st.client() {
            if let Ok(fs) = crate::api::fence::fetch_fences(client) {
                if let Some(f) = fs.first() {
                    if !f.ring.is_empty() {
                        let n = f.ring.len() as f64;
                        let sum = f.ring.iter().fold((0.0, 0.0), |a, q| (a.0 + q.0, a.1 + q.1));
                        lat = sum.0 / n;
                        lng = sum.1 / n;
                    }
                }
            }
        }
        if lat == 0.0 { lat = 30.5; lng = 104.0; }
    }

    if distance_km <= 0.0 || distance_km > 42.0 {
        return json_err(request, "距离需在 0~42 公里之间");
    }
    if duration_min <= 0.0 || duration_min > 120.0 {
        return json_err(request, "时长需在 0~120 分钟之间（超过服务端必然驳回）");
    }

    // ---- 已登录：走真实流程拿围栏和打卡点 ----
    {
        let maybe_client = {
            let st = state.lock().map_err(|e| e.to_string())?;
            if !st.logged_in() {
                None
            } else {
                st.client().cloned()
            }
        };
        if let Some(client) = maybe_client {
            let p = crate::api::flow::RunParams {
                center_lat: lat,
                center_lng: lng,
                distance_m: distance_km * 1000.0,
                duration_sec: (duration_min * 60.0).round() as i64,
                sport_type,
                run_mode,
                start_at_ms,
                custom_path: parse_custom_path(body),
                ..Default::default()
            };
            let logs: std::rc::Rc<std::cell::RefCell<Vec<String>>> =
                std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let l2 = std::rc::Rc::clone(&logs);
            let mut log = move |s: &str| l2.borrow_mut().push(s.to_string());
            let r = crate::api::flow::dry_run(&client, &p, &mut log);
            drop(log);

            if let Ok(o) = r {
                // 围栏多边形
                let fence = o.fence.as_ref().map(|f| {
                    json!({
                        "name": f.selected.name,
                        "runAreaId": f.selected.run_area_id,
                        "fallback": f.fallback,
                        "distance": f.distance_m,
                        "ring": f.selected.polygons.first().cloned().unwrap_or_default(),
                        "allPolygons": f.selected.polygons,
                    })
                });

                // 打卡点 + 是否被轨迹覆盖
                let track_xy: Vec<(f64, f64)> =
                    o.track.iter().map(|t| (t.lat, t.lon)).collect();
                let cps: Vec<Value> = o
                    .points
                    .coords
                    .iter()
                    .enumerate()
                    .map(|(i, &(a, b))| {
                        let nearest = track_xy
                            .iter()
                            .map(|q| crate::track::geo::distance((a, b), *q))
                            .fold(f64::MAX, f64::min);
                        // 高德地图按 GCJ-02 网格切瓦片，前端摆点位必须用这一套，
                        // 否则点位会整体偏出去几百米
                        let (ga, go) = o
                            .points
                            .gcj_coords
                            .get(i)
                            .copied()
                            .unwrap_or((a, b));
                        json!({
                            "index": i + 1,
                            "lat": a,
                            "lng": b,
                            "gcjLat": ga,
                            "gcjLng": go,
                            "passed": nearest < 15.0,
                            "nearest": (nearest * 10.0).round() / 10.0,
                        })
                    })
                    .collect();

                let passed = cps.iter().filter(|c| c["passed"] == true).count();
                let step = (o.track.len() / 300).max(1);
                let coords: Vec<Value> = o
                    .track
                    .iter()
                    .step_by(step)
                    // 底图是高德（GCJ-02 网格），轨迹点也要转过去才和瓦片对齐
                    .map(|t| {
                        let (gl, go) = crate::api::coord::bd09_to_gcj02(t.lat, t.lon);
                        json!([gl, go])
                    })
                    .collect();

                let ac = {
                    let c: Vec<f64> = o.sim.step_buckets.iter().map(|b| b.steps_num as f64).collect();
                    if c.len() >= 4 {
                        let m = c.iter().sum::<f64>() / c.len() as f64;
                        let den: f64 = c.iter().map(|x| (x - m).powi(2)).sum();
                        if den > 1e-9 {
                            c.windows(2).map(|w| (w[0] - m) * (w[1] - m)).sum::<f64>() / den
                        } else { 0.0 }
                    } else { 0.0 }
                };

                return json_ok(request, json!({
                    "online": true,
                    "distance": distance_km,
                    "duration": duration_min,
                    "actualLength": o.real_length_m,
                    "steps": o.total_steps,
                    "avgCadence": (o.avg_cadence * 10.0).round() / 10.0,
                    "pointCount": o.track_points,
                    "bucketCount": o.sim.step_buckets.len(),
                    "stepAutocorr": (ac * 1000.0).round() / 1000.0,
                    "blocked": o.report.blocked(),
                    "issueSummary": o.report.summary(),
                    "issues": o.report.lines(),
                    "fence": fence,
                    "checkpoints": cps,
                    "checkpointTotal": o.points.coords.len(),
                    "checkpointPassed": passed,
                    "coords": coords,
                    "mode": "online",
                    "policy": {
                        "minDistance": o.policy.min_distance,
                        "validTime": o.policy.valid_time,
                    },
                    "logs": logs.borrow().clone(),
                    "note": "已连服务端：围栏与打卡点为真实数据，未提交",
                }));
            }
            // dry_run 失败就退回本地合成，但要把原因带出来——
            // 否则页面只显示「本地合成」，完全不知道服务端哪一步卡住了
            if let Err(e) = r {
                // 把日志一起带出去。自检失败时日志里逐条写着哪个检查没过，
                // 只回一句「自检未通过」等于让用户猜。
                return json_ok(request, json!({
                    "ok": false,
                    "error": format!("真实流程失败：{e}"),
                    "logs": logs.borrow().clone(),
                }));
            }
        }
    }

    let total_m = distance_km * 1000.0;
    let total_sec = (duration_min * 60.0).round() as i64;
    let start_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // ── 模式：自由路线 or 操场绕圈 ──
    let mode = body["mode"].as_str().unwrap_or("free");
    let lane = body["lane"].as_i64().unwrap_or(1).clamp(1, 8) as u8;

    let (xy, _planner_len) = if mode == "stadium" {
        let spec = crate::track::StadiumSpec::new(lat, lng).with_lane(lane);
        let lap = crate::track::lap_length(lane);
        let laps = (total_m / lap).max(0.5);
        let pts = crate::track::stadium_track(&spec, laps, 5.0);
        let l = crate::track::geo::path_length(&pts);
        (pts, l)
    } else {
        let req = crate::track::PlanRequest {
            center_lat: lat,
            center_lng: lng,
            checkpoints: Vec::new(),
            total_distance_m: total_m,
            total_sec,
            start_ms,
            sport_type,
            sample_step_m: 5.0,
        };
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(start_ms as u64);
        let t = match crate::track::plan(&req, &mut rng) {
            Ok(t) => t,
            Err(e) => return json_err(request, &format!("轨迹规划失败: {e}")),
        };
        let pts: Vec<(f64, f64)> = t.points.iter().map(|p| (p.lat, p.lon)).collect();
        (pts, t.actual_length_m)
    };

    // ── 由速度反推时间戳 ──
    let (times, _speeds, real_len) = crate::track::retime(
        &xy,
        total_sec,
        start_ms,
        start_ms as u64,
        &crate::track::KinParams::default(),
        1.0,
        8.0,
    );
    if times.len() != xy.len() {
        return json_err(request, "时间戳生成失败");
    }
    let track_points: Vec<crate::track::TrackPoint> = xy
        .iter()
        .zip(&times)
        .map(|((a, b), t)| crate::track::TrackPoint {
            lat: *a,
            lon: *b,
            time_ms: *t,
        })
        .collect();
    let pts3: Vec<(f64, f64, i64)> = xy
        .iter()
        .zip(&times)
        .map(|((a, b), t)| (*a, *b, *t))
        .collect();

    // ── 生物力学合成 ──
    let sim_params = crate::track::SimParams {
        total_distance_m: real_len as f64,
        total_sec,
        start_ms,
        target_steps: steps,
        seed: start_ms as u64,
        ..Default::default()
    };
    let sim = match crate::track::simulate(&pts3, &sim_params) {
        Ok(s) => s,
        Err(e) => return json_err(request, &format!("数据合成失败: {e}")),
    };
    let steps = sim.total_steps;

    // ── 提交前自检 ──
    let cp_tuples: Vec<(f64, f64, String)> = Vec::new();
    let check = crate::track::Check {
        points: &track_points,
        sim: &sim,
        params: &sim_params,
        checkpoints: &cp_tuples,
        total_distance_m: real_len,
        total_time_sec: total_sec,
        total_steps: steps,
        start_time_ms: start_ms,
        stop_time_ms: start_ms + total_sec * 1000,
        pace_range: None,
        step_range: None,
    };
    let report = crate::track::inspect(&check);

    // ── 报文 ──
    //
    // 这里原来把 jitter 写死成 vec![1.0; n] 再交给 build，结果所有桶权重
    // 相同，步数退化成常数序列（一阶自相关 ≈ 0），正是 README 说「一眼假」
    // 的那种机器指纹。现在 build 直接取 sim 的桶，不再吃 jitter。
    let params = crate::api::submit::SubmitParams {
        uid: "preview".into(),
        unid: "0".into(),
        uuid: "00000000-0000-4000-8000-000000000000".into(),
        sport_type,
        total_distance_m: real_len,
        total_time_sec: total_sec,
        total_steps: steps,
        start_time_ms: start_ms,
        stop_time_ms: start_ms + total_sec * 1000,
        sel_distance_m: (real_len as f64 * 0.9) as i64,
        sel_run_time_sec: (total_sec as f64 * 0.9) as i64,
        policy: "preview".into(),
        run_area_id: 0,
        geo_fences_json: "[]".into(),
        total_ascent: 0,
    };

    let sub = match crate::api::submit::build(&params, &track_points, &[], &sim) {
        Ok(s) => s,
        Err(e) => return json_err(request, &format!("报文组装失败: {e}")),
    };
    let track = crate::track::PlanResult {
        points: track_points.clone(),
        checkpoints: Vec::new(),
        actual_length_m: real_len as f64,
    };

    // 轨迹点抽稀给前端画图（全量 400+ 点没必要都传）
    let step = (track.points.len() / 200).max(1);
    let coords: Vec<serde_json::Value> = track
        .points
        .iter()
        .step_by(step)
        .map(|p| json!([p.lat, p.lon]))
        .collect();

    let bucket_count = (total_sec / 10).max(1);
    let issues: Vec<String> = report.lines();
    let ac = {
        let c: Vec<f64> = sim.step_buckets.iter().map(|b| b.steps_num as f64).collect();
        if c.len() >= 4 {
            let m = c.iter().sum::<f64>() / c.len() as f64;
            let den: f64 = c.iter().map(|x| (x - m).powi(2)).sum();
            if den > 1e-9 {
                c.windows(2).map(|w| (w[0] - m) * (w[1] - m)).sum::<f64>() / den
            } else { 0.0 }
        } else { 0.0 }
    };
    json_ok(request, json!({
        "distance": distance_km,
        "duration": duration_min,
        "steps": steps,
        "predictedSpeed": sub.save_record["speed"],
        "predictedCalorie": sub.save_record["calorie"],
        "avgStepFreq": sub.save_record["avgStepFreq"],
        "complete": sub.save_record["complete"],
        "signature": sub.save_record["signature"],
        "payloadBytes": serde_json::to_string(&sub.save_record).map(|s| s.len()).unwrap_or(0),
        "pointCount": track.points.len(),
        "actualLength": (track.actual_length_m * 100.0).round() / 100.0,
        "bucketCount": bucket_count,
        "checkpoints": track.checkpoints.len(),
        "coords": coords,
        "mode": mode,
        "lane": lane,
        "avgCadence": (sim.avg_cadence * 10.0).round() / 10.0,
        "stepAutocorr": (ac * 1000.0).round() / 1000.0,
        "blocked": report.blocked(),
        "issueSummary": report.summary(),
        "issues": issues,
        "note": "本地预演，未提交到服务器",
    }))
}


// ---------- 记分跑：学期进度 ----------


// ---------- 账号管理 ----------

fn account_view(a: &crate::api::accounts::Account, current: bool) -> Value {
    json!({
        "id": a.id,
        "username": a.username,
        "label": a.label(),
        "name": a.name,
        "school": a.school,
        "uid": a.uid,
        "unid": a.unid,
        "loggedIn": a.logged_in(),
        "enabled": a.enabled,
        "hasPassword": !a.password_enc.is_empty(),
        "lastLoginMs": a.last_login_ms,
        "lastSubmitMs": a.last_submit_ms,
        "current": current,
    })
}

fn handle_accounts(request: tiny_http::Request, state: Shared) -> Result<(), String> {
    let st = state.lock().map_err(|e| e.to_string())?;
    let cur = st.current.clone().unwrap_or_default();
    let list: Vec<Value> = st
        .book
        .accounts
        .iter()
        .map(|a| account_view(a, a.id == cur))
        .collect();
    json_ok(request, json!({
        "count": list.len(),
        "ready": st.book.ready().len(),
        "current": cur,
        "accounts": list,
    }))
}

fn handle_add_account(
    request: tiny_http::Request,
    state: Shared,
    body: &Value,
) -> Result<(), String> {
    let username = body["username"].as_str().unwrap_or("").trim().to_string();
    let password = body["password"].as_str().unwrap_or("").to_string();
    if username.is_empty() || password.is_empty() {
        return json_err(request, "手机号和密码都要填");
    }

    // 登录前还不知道 uid，用手机号做设备作用域；登录成功后再按 uid 重建
    let mut client = Client::new(DeviceProfile::for_account(&username));
    match login(&mut client, &username, &password) {
        Ok(LoginOutcome::Ok(profile)) => {
            let mut st = state.lock().map_err(|e| e.to_string())?;
            let id = if profile.uid.is_empty() { username.clone() } else { profile.uid.clone() };
            let enc = crate::api::accounts::seal_password(&password).unwrap_or_default();
            let mut acc = crate::api::accounts::Account {
                id: id.clone(),
                username: username.clone(),
                password_enc: enc,
                ..Default::default()
            };
            if let Some(old) = st.book.get(&id) {
                acc.enabled = old.enabled;
                acc.last_submit_ms = old.last_submit_ms;
            }
            st.book.upsert(acc);
            st.apply_login(&id, &profile);
            json_ok(request, json!({
                "id": id,
                "name": profile.name,
                "school": profile.campus_name,
                "uid": profile.uid,
            }))
        }
        Ok(LoginOutcome::NeedCaptcha { uuid, message }) => {
            json_err(request, &format!("{message}（uuid={uuid}）"))
        }
        Err(e) => json_err(request, &e),
    }
}

fn handle_del_account(
    request: tiny_http::Request,
    state: Shared,
    id: &str,
    force: bool,
) -> Result<(), String> {
    // 先退出登录，再从本地删掉。顺序不能反：直接删只是丢掉本地凭据，服务端
    // 会话还活着，账号仍被标记「已登录」——下次登录触发 10121 并锁一小时，
    // 用户看到的就是「删完就锁我」。
    //
    // 关键改动：以前不管退出成功与否都照删。但「服务端没确认退出」和「服务端
    // 本来就没有活会话」在返回码上分不开，一律照删的结果是把「确保不会锁」
    // 这件事交给了运气。现在有存盘 token 且退出没成功时**先不删**，把后果
    // 交给用户确认（`?force=1` 才跳过），这样「删完锁一小时」不会再意外发生。
    let had_token = {
        let st = state.lock().map_err(|e| e.to_string())?;
        st.book
            .get(id)
            .map(|a| !a.token.is_empty())
            .unwrap_or(false)
    };

    let remote = remote_logout(&state, id);

    if had_token && !remote && !force {
        return json_ok(
            request,
            json!({
                "removed": false,
                "remoteLogout": false,
                "needConfirm": true,
                "warning": "服务端没有确认退出登录（可能是网络或接口失败），现在删掉的话服务端会话可能还在，重新登录会锁一小时。建议先点「退出」，成功了再删。",
            }),
        );
    }

    let mut st = state.lock().map_err(|e| e.to_string())?;
    let ok = st.book.remove(id);
    st.clients.remove(id);
    if st.current.as_deref() == Some(id) {
        st.current = None;
    }
    st.persist();
    json_ok(request, json!({"removed": ok, "remoteLogout": remote, "needConfirm": false}))
}

/// 按账号退出登录：只清登录态，保留手机号和存盘密码。
///
/// 和顶部的「退出」（`/api/logout`）不是一回事：那个清的是**当前**账号，
/// 而且连 web 会话一起清。这个是针对列表里某一行，退出后账号还在，
/// 状态变成「待登录」，下次点「重登」就回来了——不用重新输密码。
///
/// 同样要先调服务端 `/api/v6/user/logout`。只清本地 token 的话服务端
/// 会话还活着，账号被标记「已登录」，再登触发 10121 并锁一小时。
fn handle_logout_one(
    request: tiny_http::Request,
    state: Shared,
    id: &str,
) -> Result<(), String> {
    let remote = remote_logout(&state, id);

    let mut st = state.lock().map_err(|e| e.to_string())?;
    if st.book.get(id).is_none() {
        return json_err(request, "账号不存在");
    }
    // 只清登录态。password_enc 不动，所以退出后还能一键重登。
    if let Some(a) = st.book.get_mut(id) {
        a.token.clear();
        a.uid.clear();
        a.unid.clear();
    }
    st.clients.remove(id);
    let was_current = st.current.as_deref() == Some(id);
    if was_current {
        st.current = None;
    }
    st.persist();
    json_ok(request, json!({"ok": true, "remoteLogout": remote, "wasCurrent": was_current}))
}

fn handle_switch(
    request: tiny_http::Request,
    state: Shared,
    id: &str,
) -> Result<(), String> {
    let mut st = state.lock().map_err(|e| e.to_string())?;
    if st.book.get(id).is_none() {
        return json_err(request, "账号不存在");
    }
    st.current = Some(id.to_string());
    json_ok(request, json!({"current": id}))
}

fn handle_toggle(
    request: tiny_http::Request,
    state: Shared,
    id: &str,
) -> Result<(), String> {
    let mut st = state.lock().map_err(|e| e.to_string())?;
    let enabled = match st.book.get_mut(id) {
        Some(a) => {
            a.enabled = !a.enabled;
            a.enabled
        }
        None => return json_err(request, "账号不存在"),
    };
    st.persist();
    json_ok(request, json!({"id": id, "enabled": enabled}))
}

/// 用已存密码重新登录一个账号。
fn handle_login_one(
    request: tiny_http::Request,
    state: Shared,
    id: &str,
) -> Result<(), String> {
    let (username, password) = {
        let st = state.lock().map_err(|e| e.to_string())?;
        match st.book.get(id) {
            Some(a) => {
                if a.password_enc.is_empty() {
                    return json_err(request, "这个账号没存密码，删掉重新添加");
                }
                match crate::api::accounts::open_password(&a.password_enc) {
                    Ok(p) => (a.username.clone(), p),
                    Err(e) => return json_err(request, &e),
                }
            }
            None => return json_err(request, "账号不存在"),
        }
    };

    let mut client = Client::new(DeviceProfile::default());
    match login(&mut client, &username, &password) {
        Ok(LoginOutcome::Ok(profile)) => {
            let mut st = state.lock().map_err(|e| e.to_string())?;
            st.apply_login(id, &profile);
            json_ok(request, json!({"id": id, "name": profile.name, "uid": profile.uid}))
        }
        Ok(LoginOutcome::NeedCaptcha { message, .. }) => json_err(request, &message),
        Err(e) => json_err(request, &e),
    }
}

/// 批量登录所有账号。
fn handle_login_all(request: tiny_http::Request, state: Shared) -> Result<(), String> {
    let todo: Vec<(String, String, String)> = {
        let st = state.lock().map_err(|e| e.to_string())?;
        st.book
            .accounts
            .iter()
            .filter(|a| a.enabled && !a.password_enc.is_empty())
            .filter_map(|a| {
                crate::api::accounts::open_password(&a.password_enc)
                    .ok()
                    .map(|p| (a.id.clone(), a.username.clone(), p))
            })
            .collect()
    };

    if todo.is_empty() {
        return json_err(request, "没有可登录的账号（需要已存密码且启用）");
    }

    let mut results = Vec::new();
    for (i, (id, user, pass)) in todo.iter().enumerate() {
        if i > 0 {
            // 错开，避免同 IP 短时间连续登录触发风控
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
        let mut client = Client::new(DeviceProfile::default());
        match login(&mut client, user, pass) {
            Ok(LoginOutcome::Ok(profile)) => {
                let mut st = state.lock().map_err(|e| e.to_string())?;
                st.apply_login(id, &profile);
                results.push(json!({"id": id, "ok": true, "name": profile.name}));
            }
            Ok(LoginOutcome::NeedCaptcha { message, .. }) => {
                results.push(json!({"id": id, "ok": false, "error": message}));
            }
            Err(e) => results.push(json!({"id": id, "ok": false, "error": e})),
        }
    }

    let ok = results.iter().filter(|r| r["ok"] == true).count();
    json_ok(request, json!({"total": results.len(), "ok": ok, "results": results}))
}

/// 批量刷：所有就绪账号依次跑一遍。
fn handle_batch(
    request: tiny_http::Request,
    state: Shared,
    body: &Value,
) -> Result<(), String> {
    let distance_km = body["distance"].as_f64().unwrap_or(2.0);
    let duration_min = body["duration"].as_f64().unwrap_or(12.0);
    let run_mode = body["runMode"].as_i64().unwrap_or(1);
    let start_at_ms = body["startAtMs"].as_i64().filter(|v| *v > 0);
    let sport_type = body["sportType"].as_i64().unwrap_or(1) as i32;
    let dry = body["dry"].as_bool().unwrap_or(false);
    // 账号间隔（毫秒），默认 8 秒，避免同 IP 高频
    let gap_ms = body["gapMs"].as_u64().unwrap_or(8000);
    let only: Vec<String> = body["ids"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();

    if distance_km <= 0.0 || distance_km > 42.0 {
        return json_err(request, "距离需在 0~42 公里之间");
    }
    if duration_min <= 0.0 || duration_min > 120.0 {
        return json_err(request, "时长需在 0~120 分钟之间（超过服务端必然驳回）");
    }

    // 挑出要跑的账号
    let targets: Vec<String> = {
        let st = state.lock().map_err(|e| e.to_string())?;
        st.book
            .ready()
            .iter()
            .filter(|a| only.is_empty() || only.contains(&a.id))
            .map(|a| a.id.clone())
            .collect()
    };
    if targets.is_empty() {
        return json_err(request, "没有可跑的账号（需要已登录且启用）");
    }

    let mut results = Vec::new();
    for (i, id) in targets.iter().enumerate() {
        if i > 0 && gap_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(gap_ms));
        }

        // 每个账号自己一份 client 快照
        let client = {
            let st = state.lock().map_err(|e| e.to_string())?;
            st.client_of(id).cloned()
        };
        let Some(client) = client else {
            results.push(json!({"id": id, "ok": false, "error": "无会话"}));
            continue;
        };

        // 静默窗口
        let since = {
            let st = state.lock().map_err(|e| e.to_string())?;
            st.book
                .get(id)
                .map(|a| a.since_submit_sec(crate::crypto::envelope::now_ms() as i64))
                .unwrap_or(i64::MAX)
        };
        if !dry && since < crate::api::flow::SILENCE_SEC {
            results.push(json!({
                "id": id, "ok": false,
                "error": format!("距上次提交 {since}s，静默窗口 {}s 未过", crate::api::flow::SILENCE_SEC)
            }));
            continue;
        }

        let p = crate::api::flow::RunParams {
            distance_m: distance_km * 1000.0,
            duration_sec: (duration_min * 60.0).round() as i64,
            sport_type,
            run_mode,
            start_at_ms,
            ..Default::default()
        };

        let logs = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let l2 = std::rc::Rc::clone(&logs);
        let mut log = move |s: &str| l2.borrow_mut().push(s.to_string());

        let outcome = if dry {
            crate::api::flow::dry_run(&client, &p, &mut log)
        } else {
            let mut confirm = |_o: &crate::api::flow::RunOutcome| true;
            crate::api::flow::run_full(&client, &p, &mut log, &mut confirm)
        };
        drop(log);

        match outcome {
            Ok(o) => {
                if o.submitted {
                    let mut st = state.lock().map_err(|e| e.to_string())?;
                    if let Some(a) = st.book.get_mut(id) {
                        a.last_submit_ms = crate::crypto::envelope::now_ms() as i64;
                    }
                    st.persist();
                }
                results.push(json!({
                    "id": id,
                    "ok": true,
                    "submitted": o.submitted,
                    "recordId": o.record_id,
                    "distance": o.real_length_m,
                    "steps": o.total_steps,
                    "logs": logs.borrow().clone(),
                }));
            }
            Err(e) => results.push(json!({
                "id": id, "ok": false, "error": e,
                "logs": logs.borrow().clone(),
            })),
        }
    }

    let ok = results.iter().filter(|r| r["ok"] == true).count();
    let submitted = results.iter().filter(|r| r["submitted"] == true).count();
    json_ok(request, json!({
        "total": results.len(),
        "ok": ok,
        "submitted": submitted,
        "results": results,
    }))
}


// ---------- 补跑时刻 ----------

/// 算补跑时刻。查询参数：`mode`=at|ago|rand，`hhmm`、`days`、`minutes`、`dur`。
///
/// 只算不跑，前端拿到结果给用户确认；确认后再带着 `startAtMs` 调 `/api/run`。
fn handle_starttimes(request: tiny_http::Request) -> Result<(), String> {
    use crate::api::start_time as st;

    let q = request.url().split_once('?').map(|x| x.1).unwrap_or("");
    let get = |k: &str| -> Option<String> {
        q.split('&').find_map(|kv| {
            let (a, b) = kv.split_once('=')?;
            if a == k {
                Some(b.replace("%3A", ":").replace("%20", " ").to_string())
            } else {
                None
            }
        })
    };

    let mode = get("mode").unwrap_or_else(|| "now".into());
    let dur_s: i64 = get("dur").and_then(|v| v.parse().ok()).unwrap_or(720);
    let days: i64 = get("days").and_then(|v| v.parse().ok()).unwrap_or(0);
    let now = crate::crypto::envelope::now_ms() as i64;
    let tz = st::default_tz_offset_min();

    let result = match mode.as_str() {
        "at" => {
            let hhmm = get("hhmm").unwrap_or_else(|| "07:30".into());
            match st::parse_hhmm(&hhmm) {
                Ok((h, m)) => st::specified(now, days, h, m, dur_s),
                Err(e) => return json_err(request, &e),
            }
        }
        "ago" => {
            let mins: i64 = get("minutes").and_then(|v| v.parse().ok()).unwrap_or(60);
            st::ago(now, mins, dur_s)
        }
        "rand" => {
            use rand::Rng;
            let r: f64 = rand::thread_rng().gen();
            st::random_in_day(now, days, dur_s, r)
        }
        // 现在跑就是现在跑，不参与钳制（前端也不会拿它去弹确认框）
        "now" => Ok(st::StartTime { ms: now, clamped: false }),
        other => return json_err(request, &format!("未知模式 {other}")),
    };

    match result {
        Ok(x) => json_ok(request, json!({
            "mode": mode,
            "startMs": x.ms,
            "stopMs": x.stop_ms(dur_s),
            "startText": st::format_ms(x.ms, tz),
            "stopText": st::format_ms(x.stop_ms(dur_s), tz),
            "clamped": x.clamped,
            "durationSec": dur_s,
            "nowText": st::format_ms(now, tz),
            "limits": {
                "maxDaysAgo": st::MAX_DAYS_AGO,
                "maxAgoMin": st::MAX_AGO_MIN,
                "randHourLo": st::RAND_HOUR_LO,
                "randHourHi": st::RAND_HOUR_HI,
            },
        })),
        Err(e) => json_err(request, &e),
    }
}

fn handle_semester(request: tiny_http::Request, state: Shared) -> Result<(), String> {
    // 先把 client 克隆出来再放锁。Client 是 Clone，网络请求几十秒，
    // 持着 Mutex 做 IO 会让 /api/accounts 之类的请求全部排队，
    // 页面看起来就是「服务假死」。
    let client = {
        let st = state.lock().map_err(|e| e.to_string())?;
        if !st.logged_in() {
            return json_err(request, "请先登录");
        }
        let Some(c) = st.client() else {
            return json_err(request, "当前账号没有可用会话，请重新登录");
        };
        c.clone()
    };

    let mut out = serde_json::Map::new();
    // 原始响应也带上，字段名不对时能直接看。
    // 这里必须和 fetch_summary 一样带 sid：空 body 服务端回的是空学期
    // （sname=""、全部 0），和下面的 summary 对不上。
    let sid = crate::api::semester::fetch_goal(&client, 1)
        .ok()
        .map(|g| g.sid)
        .filter(|s| *s > 0);
    let raw_body = match sid {
        Some(s) => json!({ "sid": s }),
        None => json!({}),
    };
    if let Ok(raw) = client.request(
        "POST",
        &format!("{}{}", crate::crypto::consts::hosts::RUN, crate::api::semester::SUMMARY_PATH),
        Some(&raw_body),
    ) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(d) = v.get("data") {
                out.insert("summaryRaw".into(), d.clone());
            }
        }
    }

    match crate::api::semester::fetch_summary(&client) {
        Ok(sm) => {
            out.insert("summary".into(), json!({
                "sname": sm.sname,
                "semesterDis": sm.semester_dis,
                "semesterValidDis": sm.semester_valid_dis,
                "semesterCount": sm.semester_count,
                "semesterValidCount": sm.semester_valid_count,
                "validRatio": sm.valid_ratio(),
                "text": sm.summary(),
            }));
        }
        Err(e) => {
            out.insert("summaryError".into(), json!(e));
        }
    }
    if let Ok(raw) = client.request(
        "POST",
        &format!("{}{}", crate::crypto::consts::hosts::RUN, crate::api::semester::PERSONAL_PATH),
        Some(&json!({"runMode": 1})),
    ) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(d) = v.get("data") {
                out.insert("goalRaw".into(), d.clone());
            }
        }
    }

    match crate::api::semester::fetch_goal(&client, 1) {
        Ok(g) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            out.insert("goal".into(), json!({
                "targetDis": g.target_dis,
                "targetCount": g.target_count,
                "daysLeft": g.days_left(now),
                "inTerm": g.in_term(now),
            }));
        }
        Err(e) => {
            out.insert("goalError".into(), json!(e));
        }
    }
    json_ok(request, Value::Object(out))
}

// ---------- 围栏列表（地图上画出来） ----------

fn handle_fences(
    request: tiny_http::Request,
    state: Shared,
    _query: &str,
) -> Result<(), String> {
    // 先把 client 克隆出来再放锁。Client 是 Clone，网络请求几十秒，
    // 持着 Mutex 做 IO 会让 /api/accounts 之类的请求全部排队，
    // 页面看起来就是「服务假死」。
    let client = {
        let st = state.lock().map_err(|e| e.to_string())?;
        if !st.logged_in() {
            return json_err(request, "请先登录");
        }
        let Some(c) = st.client() else {
            return json_err(request, "当前账号没有可用会话，请重新登录");
        };
        c.clone()
    };
    match crate::api::fence::fetch_fences(&client) {
        Ok(fs) => {
            let list: Vec<Value> = fs
                .iter()
                .map(|f| json!({
                    "id": f.id,
                    "name": f.name,
                    "ring": f.ring,
                }))
                .collect();
            json_ok(request, json!({"count": list.len(), "fences": list}))
        }
        Err(e) => json_err(request, &format!("拉围栏失败: {e}")),
    }
}

// ---------- 真跑：提交 ----------

/// 解析手绘轨迹。
///
/// 前端底图是高德，瓦片按 GCJ-02 网格切，Leaflet 拿到手的就是 GCJ-02，
/// 提交报文要 BD-09，所以逐点 `gcj02_to_bd09`。
/// 少于 2 个点当作没传，退回算法规划。
fn parse_custom_path(body: &Value) -> Option<Vec<(f64, f64)>> {
    body.get("customPath")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let pair = item.as_array()?;
                    let lat = pair.first()?.as_f64()?;
                    let lng = pair.get(1)?.as_f64()?;
                    // 明显不是经纬度的值直接丢掉，别把脏数据带进报文
                    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
                        return None;
                    }
                    Some(crate::api::coord::gcj02_to_bd09(lat, lng))
                })
                .collect::<Vec<(f64, f64)>>()
        })
        .filter(|v| v.len() >= 2)
}

fn handle_run(
    request: tiny_http::Request,
    state: Shared,
    body: &Value,
) -> Result<(), String> {
    let distance_km = body["distance"].as_f64().unwrap_or(2.0);
    let duration_min = body["duration"].as_f64().unwrap_or(12.0);
    let lat = body["lat"].as_f64();
    let lng = body["lng"].as_f64();
    let run_mode = body["runMode"].as_i64().unwrap_or(1);
    let start_at_ms = body["startAtMs"].as_i64().filter(|v| *v > 0);
    let sport_type = body["sportType"].as_i64().unwrap_or(1) as i32;

    if distance_km <= 0.0 || distance_km > 42.0 {
        return json_err(request, "距离需在 0~42 公里之间");
    }
    if duration_min <= 0.0 || duration_min > 120.0 {
        return json_err(request, "时长需在 0~120 分钟之间（超过服务端必然驳回）");
    }

    // 先克隆 client 再放锁——后面 fetch_fences 是网络请求，持着 Mutex
    // 做 IO 会让 /api/accounts 之类的请求全部排队，页面看起来像服务死了。
    let client = {
        let st = state.lock().map_err(|e| e.to_string())?;
        if !st.logged_in() {
            return json_err(request, "请先登录");
        }
        let Some(c) = st.client() else {
            return json_err(request, "当前账号没有可用会话，请重新登录");
        };
        c.clone()
    };

    // 中心点：没传就用围栏中心反推
    let (clat, clng) = match (lat, lng) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            match crate::api::fence::fetch_fences(&client) {
                Ok(fs) if !fs.is_empty() && !fs[0].ring.is_empty() => {
                    let r = &fs[0].ring;
                    let n = r.len() as f64;
                    let s: (f64, f64) = r.iter().fold((0.0, 0.0), |a, q| (a.0 + q.0, a.1 + q.1));
                    (s.0 / n, s.1 / n)
                }
                _ => (30.5, 104.0),
            }
        }
    };

    let custom_path = parse_custom_path(body);

    let p = crate::api::flow::RunParams {
        center_lat: clat,
        center_lng: clng,
        distance_m: distance_km * 1000.0,
        duration_sec: (duration_min * 60.0).round() as i64,
        sport_type,
        run_mode,
        start_at_ms,
        custom_path,
        ..Default::default()
    };

    // 日志用共享容器，闭包借用结束后还能读
    let logs: std::rc::Rc<std::cell::RefCell<Vec<String>>> =
        std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let logs2 = std::rc::Rc::clone(&logs);
    let mut log = move |s: &str| logs2.borrow_mut().push(s.to_string());
    let mut confirm = |_o: &crate::api::flow::RunOutcome| true;

    let result = crate::api::flow::run_full(&client, &p, &mut log, &mut confirm);
    drop(log);

    match result {
        Ok(o) => {
            // 记下这次提交的时刻。
            //
            // 这个字段原来从没被更新过——`last_submit_ms` 只在切换账号时
            // 保留旧值，永远是 0。于是静默窗口判断 `now - last_submit >= 600s`
            // 恒成立，客户端这边等于完全不拦；连续点几下全发给服务端，
            // 每次都要等它回 10603 才知道太频繁。
            if o.submitted {
                if let Ok(mut st) = state.lock() {
                    if let Some(id) = st.current.clone() {
                        let now = crate::crypto::envelope::now_ms() as i64;
                        if let Some(a) = st.book.get_mut(&id) {
                            a.last_submit_ms = now;
                        }
                        st.persist();
                    }
                }
            }
            json_ok(request, json!({
            "submitted": o.submitted,
            "recordId": o.record_id,
            "distance": o.real_length_m,
            "steps": o.total_steps,
            "cadence": o.avg_cadence,
            "points": o.track_points,
            "obsKeys": o.obs_keys.len(),
            "logs": logs.borrow().clone(),
            }))
        }
        Err(e) => json_err(request, &format!("{e}\n---\n{}", logs.borrow().join("\n"))),
    }
}

/// 按 rrid 从 OBS 拉历史记录的原始轨迹。
///
/// 轨迹不在 `get_one_record` 里返回（实测 50 条记录该字段全为 null），
/// 但记录列表带了 uuid 和 startTime，拼出 objectKey 就能直接从 OBS 取。
/// App 自己走不走这条路还没定论，见 TRAJECTORY-DISPLAY.md。
/// 读本地手绘禁区。
fn handle_blockers_get(request: tiny_http::Request) -> Result<(), String> {
    let polys = crate::track::roadnet::load_local_blockers();
    json_ok(
        request,
        json!({
            "count": polys.len(),
            "polys": polys
                .iter()
                .map(|p| p.iter().map(|q| vec![q.0, q.1]).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        }),
    )
}

/// 保存本地手绘禁区。body: {"polys":[[[lat,lng],...], ...]}
///
/// 用法：OSM 在国内校园没有建筑轮廓，规划出来的轨迹会从楼里穿过去。
/// 在地图上把楼圈成矩形，存一份，之后所有规划都会绕开。
fn handle_blockers_put(request: tiny_http::Request, body: &Value) -> Result<(), String> {
    let Some(arr) = body.get("polys").and_then(|v| v.as_array()) else {
        return json_err(request, "polys 必须是数组");
    };
    if arr.len() > 200 {
        return json_err(request, "禁区最多 200 块");
    }
    let mut polys: Vec<Vec<(f64, f64)>> = Vec::new();
    for poly in arr {
        let Some(pts) = poly.as_array() else {
            return json_err(request, "每块禁区要是 [[lat,lng], ...]");
        };
        let pts: Vec<(f64, f64)> = pts
            .iter()
            .filter_map(|p| {
                let a = p.as_array()?;
                Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
            })
            .collect();
        if pts.len() < 3 {
            return json_err(request, "每块禁区至少要 3 个点");
        }
        for p in &pts {
            if !(-90.0..=90.0).contains(&p.0) || !(-180.0..=180.0).contains(&p.1) {
                return json_err(request, "坐标超出经纬度范围");
            }
        }
        polys.push(pts);
    }
    match crate::track::roadnet::save_local_blockers(&polys) {
        Ok(()) => json_ok(request, json!({"saved": polys.len()})),
        Err(e) => json_err(request, &e),
    }
}

fn handle_obs_fetch(
    request: tiny_http::Request,
    state: Shared,
    body: &Value,
) -> Result<(), String> {
    let key = match body.get("key").and_then(|v| v.as_str()) {
        Some(k) if k.starts_with("run_data/") || k.starts_with("runData/") => k.to_string(),
        _ => return json_err(request, "key 参数缺失或格式不对"),
    };
    let client = {
        let st = state.lock().map_err(|e| e.to_string())?;
        if !st.logged_in() {
            return json_err(request, "请先登录");
        }
        let Some(c) = st.client() else {
            return json_err(request, "当前账号没有可用会话，请重新登录");
        };
        c.clone()
    };
    match crate::api::obs::fetch_object(&client, &key) {
        Ok(v) => json_ok(request, v),
        Err(e) => json_err(request, &e),
    }
}

/// 透传任意业务接口，用于探测没写进代码的接口。
///
/// body: {"path":"/api/v670/...","body":{...},"host":"run|discovery"}
fn handle_raw(
    request: tiny_http::Request,
    state: Shared,
    body: &Value,
) -> Result<(), String> {
    let path = body.get("path").and_then(|v| v.as_str()).unwrap_or("");
    if !path.starts_with("/api/") {
        return json_err(request, "path 必须是 /api/ 开头");
    }
    let inner = body.get("body").cloned().unwrap_or_else(|| json!({}));
    let host = match body.get("host").and_then(|v| v.as_str()).unwrap_or("run") {
        "discovery" => crate::crypto::consts::hosts::DISCOVERY,
        _ => crate::crypto::consts::hosts::RUN,
    };

    let client = {
        let st = state.lock().map_err(|e| e.to_string())?;
        if !st.logged_in() {
            return json_err(request, "请先登录");
        }
        let Some(c) = st.client() else {
            return json_err(request, "当前账号没有可用会话，请重新登录");
        };
        c.clone()
    };
    let method = match body.get("method").and_then(|v| v.as_str()).unwrap_or("POST") {
        "GET" => "GET",
        _ => "POST",
    };
    let full = if method == "GET" && !inner.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        let qs: Vec<String> = inner.as_object().map(|o| {
            o.iter().filter_map(|(k, v)| match v {
                Value::String(sv) => Some(format!("{k}={}", sv.replace(' ', "%20"))),
                Value::Number(n) => Some(format!("{k}={n}")),
                _ => None,
            }).collect()
        }).unwrap_or_default();
        if qs.is_empty() { format!("{host}{path}") } else { format!("{host}{path}?{}", qs.join("&")) }
    } else {
        format!("{host}{path}")
    };
    let payload = if method == "GET" { None } else { Some(&inner) };
    match client.request(method, &full, payload) {
        Ok(raw) => {
            let v: Value = serde_json::from_str(&raw)
                .unwrap_or_else(|_| json!({"raw_text": raw.chars().take(2000).collect::<String>()}));
            json_ok(request, v)
        }
        Err(e) => json_err(request, &e),
    }
}

fn handle_records(request: tiny_http::Request, state: Shared, query: &str) -> Result<(), String> {
    let page = query_param(query, "page").and_then(|v| v.parse().ok()).unwrap_or(1u32);
    let size = query_param(query, "size").and_then(|v| v.parse().ok()).unwrap_or(20u32);
    let sport = query_param(query, "type").and_then(|v| v.parse().ok()).unwrap_or(1u32);

    // 先把 client 克隆出来再放锁。Client 是 Clone，网络请求几十秒，
    // 持着 Mutex 做 IO 会让 /api/accounts 之类的请求全部排队，
    // 页面看起来就是「服务假死」。
    let client = {
        let st = state.lock().map_err(|e| e.to_string())?;
        if !st.logged_in() {
            return json_err(request, "请先登录");
        }
        let Some(c) = st.client() else {
            return json_err(request, "当前账号没有可用会话，请重新登录");
        };
        c.clone()
    };
    match records::list_records(&client, page, size, sport) {
        Ok(data) => json_ok(request, data),
        Err(e) => json_err(request, &e),
    }
}

fn handle_record_detail(
    request: tiny_http::Request,
    state: Shared,
    rrid: i64,
) -> Result<(), String> {
    // 先把 client 克隆出来再放锁。Client 是 Clone，网络请求几十秒，
    // 持着 Mutex 做 IO 会让 /api/accounts 之类的请求全部排队，
    // 页面看起来就是「服务假死」。
    let client = {
        let st = state.lock().map_err(|e| e.to_string())?;
        if !st.logged_in() {
            return json_err(request, "请先登录");
        }
        let Some(c) = st.client() else {
            return json_err(request, "当前账号没有可用会话，请重新登录");
        };
        c.clone()
    };
    match records::get_record(&client, rrid) {
        Ok(data) => json_ok(request, data),
        Err(e) => json_err(request, &e),
    }
}

// ---------- HTTP 小工具 ----------

fn read_json(request: &mut tiny_http::Request) -> Result<Value, String> {
    let mut s = String::new();
    let r = request.as_reader();
    r.read_to_string(&mut s).map_err(|e| e.to_string())?;
    serde_json::from_str(&s).map_err(|e| format!("请求体不是 JSON: {e}"))
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let mut it = pair.splitn(2, '=');
        let k = it.next()?;
        if k == key {
            it.next().map(|v| v.to_string())
        } else {
            None
        }
    })
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("header 构造失败")
}

fn respond_json(
    request: tiny_http::Request,
    status: u16,
    body: Value,
) -> Result<(), String> {
    let s = serde_json::to_string(&body).unwrap_or_else(|_| "{}".into());
    let resp = Response::from_string(s)
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json; charset=utf-8"));
    request.respond(resp).map_err(|e| e.to_string())
}

fn json_ok(request: tiny_http::Request, body: Value) -> Result<(), String> {
    respond_json(request, 200, json!({"ok": true, "data": body}))
}

fn json_err(request: tiny_http::Request, msg: &str) -> Result<(), String> {
    respond_json(request, 200, json!({"ok": false, "error": msg}))
}

fn json_err_code(request: tiny_http::Request, code: u16, msg: &str) -> Result<(), String> {
    respond_json(request, code, json!({"ok": false, "error": msg}))
}

fn respond_html(request: tiny_http::Request, html: &str) -> Result<(), String> {
    let resp = Response::from_string(html)
        .with_header(header("Content-Type", "text/html; charset=utf-8"))
        // 不设这个头，浏览器会一直用缓存的旧页面：改了按钮逻辑用户也看不到，
        // 点了还是旧行为（表现为「退出键没反应」）。
        .with_header(header("Cache-Control", "no-store, no-cache, must-revalidate, max-age=0"))
        .with_header(header("Pragma", "no-cache"))
        .with_header(header("Expires", "0"));
    request.respond(resp).map_err(|e| e.to_string())
}

const INDEX_HTML: &str = include_str!("index.html");

#[cfg(test)]
mod device_scope_tests {
    use super::device_scope;

    /// 登录前后必须算出同一个设备作用域。
    ///
    /// 登录前只知道手机号、登录后只知道 uid。两者若映射到不同 scope，
    /// 就会生成两套 device_id，服务端当成两台手机，触发 10121
    /// 「此账号已在其他设备登录」并把账号锁一小时。
    #[test]
    fn device_scope_is_stable_across_login() {
        assert_eq!(device_scope("13900000000"), "13900000000");
        assert_eq!(device_scope("99999"), "99999");
        for _ in 0..3 {
            assert_eq!(device_scope("13900000000"), device_scope("13900000000"));
        }
    }
}

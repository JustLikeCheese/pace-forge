//! 入口：`pace-forge <命令>`
//!
//! 命令：
//! - `web [addr]`      启动 Web 控制台
//! - `dry <距离> <分钟>` 只做流程预演，不发写请求
//! - `stadium <圈数>`   生成操场轨迹并打印参数
//! - `version` / `help`


/// 解析必填或可缺省的数字参数。
///
/// 之前一律 `.and_then(parse().ok()).unwrap_or(默认值)`，于是
/// `run 2 abc` 会被当成 12 分钟**真提交**，`check -1 12` 带着负距离跑完全程，
/// `check 1e9 1e9` 直接 OOM abort。打错字和没写是两回事：
/// 参数存在但解析不出来 → 报错退出；参数不存在 → 用默认值。
fn parse_num(args: &[String], idx: usize, default: f64, name: &str) -> f64 {
    match args.get(idx) {
        None => default,
        Some(raw) => match raw.parse::<f64>() {
            Ok(v) if v.is_finite() => v,
            _ => {
                eprintln!("参数错误：{name} 「{raw}」不是有效数字");
                std::process::exit(2);
            }
        },
    }
}

/// 校验公里/分钟，和库层 `RunParams::validate` 保持同一套上限。
fn checked_km_min(dist_km: f64, dur_min: f64) -> (f64, f64) {
    if !(0.0..=42.0).contains(&dist_km) {
        eprintln!("参数错误：距离要在 0~42 公里之间，收到 {dist_km}");
        std::process::exit(2);
    }
    if !(0.0..=120.0).contains(&dur_min) {
        eprintln!("参数错误：时长要在 0~120 分钟之间，收到 {dur_min}");
        std::process::exit(2);
    }
    (dist_km, dur_min)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("web");

    match cmd {
        "web" => {
            let addr = args.get(1).cloned().unwrap_or_else(|| "127.0.0.1:9999".into());
            if let Err(e) = pace_forge::web::serve(&addr) {
                eprintln!("启动失败: {e}");
                std::process::exit(1);
            }
        }
        "semester" => show_semester(),
        "daily" => do_daily(&args[1..]),
        "export-ci" => do_export_ci(),
        "logout" => do_logout(),
        "relogin" => do_relogin(),
        "check" => {
            let dist_km = parse_num(&args, 1, 2.0, "距离(km)");
            let dur_min = parse_num(&args, 2, 12.0, "时长(min)");
            let (dist_km, dur_min) = checked_km_min(dist_km, dur_min);
            local_check(dist_km, dur_min);
        }
        "run" => {
            let dist_km = parse_num(&args, 1, 2.0, "距离(km)");
            let dur_min = parse_num(&args, 2, 12.0, "时长(min)");
            let (dist_km, dur_min) = checked_km_min(dist_km, dur_min);
            let yes = args.iter().any(|a| a == "-y" || a == "--yes");
            do_run(dist_km, dur_min, yes);
        }
        "dry" => {
            let dist_km = parse_num(&args, 1, 2.0, "距离(km)");
            let dur_min = parse_num(&args, 2, 12.0, "时长(min)");
            let (dist_km, dur_min) = checked_km_min(dist_km, dur_min);
            dry_run(dist_km, dur_min);
        }
        "at" => {
            // at <HH:MM> <km> <min> [--days N]
            let hhmm = args.get(1).cloned().unwrap_or_default();
            let dist_km = parse_num(&args, 2, 2.0, "距离(km)");
            let dur_min = parse_num(&args, 3, 12.0, "时长(min)");
            let (dist_km, dur_min) = checked_km_min(dist_km, dur_min);
            let days: i64 = args
                .iter()
                .position(|a| a == "--days")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let yes = args.iter().any(|a| a == "-y" || a == "--yes");
            do_at(&hhmm, days, dist_km, dur_min, yes);
        }
        "ago" => {
            // ago <分钟> <km> <min>
            let mins: i64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(60);
            let dist_km = parse_num(&args, 2, 2.0, "距离(km)");
            let dur_min = parse_num(&args, 3, 12.0, "时长(min)");
            let (dist_km, dur_min) = checked_km_min(dist_km, dur_min);
            let yes = args.iter().any(|a| a == "-y" || a == "--yes");
            do_ago(mins, dist_km, dur_min, yes);
        }
        "rand" => {
            // rand <km> <min> [--days N]
            let dist_km: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2.0);
            let dur_min: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(12.0);
            let days: i64 = args
                .iter()
                .position(|a| a == "--days")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let yes = args.iter().any(|a| a == "-y" || a == "--yes");
            do_rand(days, dist_km, dur_min, yes);
        }
        "stadium" => {
            let laps: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5.0);
            let lane: u8 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
            stadium_probe(laps, lane);
        }
        "version" | "-V" | "--version" => {
            println!("pace-forge {}", pace_forge::VERSION);
        }
        "help" | "-h" | "--help" => {
            println!("pace-forge {} — 运动世界校园协议工具\n", pace_forge::VERSION);
            println!("用法:");
            println!("  pace-forge web [addr]          启动 Web 控制台（默认 127.0.0.1:9998）");
            println!("  pace-forge semester            查学期进度（记分跑）");
            println!("  pace-forge run [km] [min] [-y] 刷跑并提交（-y 跳过确认）");
            println!("  pace-forge check [km] [min]    离线体检轨迹（不联网、不登录）");
            println!("  pace-forge dry [km] [min]      只预演，不提交（要登录）");
            println!("  pace-forge at <HH:MM> [km] [min] [--days N]  补跑：指定当天/往前的时刻");
            println!("  pace-forge ago <分钟> [km] [min]            补跑：N 分钟前");
            println!("  pace-forge rand [km] [min] [--days N]       补跑：当天 7-20 点随机时刻");
            println!("  pace-forge stadium [圈] [道次] 操场轨迹参数");
            println!("  pace-forge daily [--dry] [--result F] [--delay-max-min N] [--gap-sec A-B] [--only 1,3]");
            println!("                                 每日批量（账号读环境变量 PACE_ACCOUNTS）");
            println!("  pace-forge export-ci           从本机账号库导出 PACE_ACCOUNTS（含密码，只写 stdout）");
            println!("  pace-forge version             版本号");
            println!();
            println!("规则：开始 + 用时 + 5s 余量 ≤ 现在，最多往前补 3 天。");
            println!("run/dry/at/ago/rand 需要已保存的登录会话（$HOME/.pace-forge/session.json）。");
        }
        other => {
            eprintln!("未知命令: {other}\n用 `pace-forge help` 查看用法");
            std::process::exit(2);
        }
    }
}

fn dry_run(dist_km: f64, dur_min: f64) {
    use pace_forge::api::{flow, session};

    let sess = session::load_session();
    if !sess.is_valid() {
        eprintln!("未登录：先通过 Web 控制台登录一次（会话存在 $HOME/.pace-forge/session.json）");
        std::process::exit(1);
    }
    let client = pace_forge::api::client::Client::new(pace_forge::api::client::DeviceProfile::default())
        .with_credentials(pace_forge::api::client::Credentials {
            uid: Some(sess.uid.to_string()),
            token: Some(sess.token.clone()),
            unid: Some(sess.unid.clone()),
        });

    // 中心点必须从真实围栏反推。
    //
    // CLI 原来走 RunParams::default()，center 是 (0.0, 0.0)——赤道的几内亚湾。
    // 围栏选择按「最近」兜底，会选中 2677 公里外的学校，再交给
    // route_through_in_order 就算出 7640 公里、152 万个点的鬼轨迹。
    let center: Option<(f64, f64)> = match pace_forge::api::fence::fetch_fences(&client) {
        Ok(fs) if !fs.is_empty() => fs[0].ring.first().map(|_| {
            let n = fs[0].ring.len() as f64;
            let s = fs[0]
                .ring
                .iter()
                .fold((0.0, 0.0), |a, q| (a.0 + q.0, a.1 + q.1));
            (s.0 / n, s.1 / n)
        }),
        Ok(_) => None,
        Err(e) => {
            eprintln!("拉围栏失败: {e}");
            None
        }
    };
    let (clat, clng) = match center {
        Some(c) => c,
        None => {
            eprintln!("拿不到围栏中心点，无法规划轨迹");
            std::process::exit(1);
        }
    };
    println!("围栏中心: ({clat:.5}, {clng:.5})");

    let p = flow::RunParams {
        center_lat: clat,
        center_lng: clng,
        distance_m: dist_km * 1000.0,
        duration_sec: (dur_min * 60.0).round() as i64,
        ..Default::default()
    };

    println!(
        "预演：{:.1} km / {:.0} 分钟（不发任何写请求）\n",
        dist_km, dur_min
    );
    let mut log = |s: &str| println!("{s}");
    match flow::dry_run(&client, &p, &mut log) {
        Ok(o) => {
            println!("\n=== 完成 ===");
            println!("  规划点数    {}", o.track_points);
            println!("  实际里程    {:.1} m", o.actual_length_m);
            println!("  总步数      {}", o.total_steps);
            println!("  平均步频    {:.1} 步/分", o.avg_cadence);
            println!("  自检        {}", o.report.summary());
            println!("  已提交      {}", if o.submitted { "是" } else { "否（预演）" });
        }
        Err(e) => {
            eprintln!("\n预演失败: {e}");
            std::process::exit(1);
        }
    }
}

/// 完全离线的轨迹体检：不登录、不联网，只看本地合成的数据像不像真跑。
/// 每日批量：账号从环境变量 `PACE_ACCOUNTS` 读，结果写 `--result`。
/// 有账号失败时退出码 1（跳过不算失败）。
fn do_daily(args: &[String]) {
    use pace_forge::api::daily::{run_daily, DailyConfig, DailyOptions};
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let mut opt = DailyOptions { dry: args.iter().any(|a| a == "--dry"), ..Default::default() };
    if let Some(m) = flag("--delay-max-min").and_then(|s| s.parse::<u64>().ok()) {
        opt.start_delay_max_s = m * 60;
    }
    if let Some(g) = flag("--gap-sec") {
        let mut it = g.split('-').filter_map(|x| x.trim().parse::<u64>().ok());
        if let (Some(a), Some(b)) = (it.next(), it.next()) {
            opt.gap_s = (a.min(b), a.max(b));
        }
    }
    if let Some(o) = flag("--only") {
        opt.only = o.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    }
    let raw = std::env::var("PACE_ACCOUNTS").unwrap_or_default();
    let cfg: DailyConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("PACE_ACCOUNTS 缺失或格式不对: {e}");
            if let Some(p) = flag("--result") {
                let _ = std::fs::write(p, r#"{"error_code":"configuration_failed","accounts":[]}"#);
            }
            std::process::exit(1);
        }
    };
    let res = run_daily(&cfg, &opt);
    if let Some(p) = flag("--result") {
        if let Err(e) = std::fs::write(&p, serde_json::to_string_pretty(&res).unwrap_or_default()) {
            eprintln!("写结果失败: {e}");
        }
    }
    let accts = res["accounts"].as_array().cloned().unwrap_or_default();
    let ok = accts.iter().filter(|a| a["status"] == "success").count();
    let skip = accts.iter().filter(|a| a["status"] == "skipped").count();
    let fail = accts.len() - ok - skip;
    println!("完成：成功 {ok} · 跳过 {skip} · 失败 {fail}");
    if fail > 0 {
        std::process::exit(1);
    }
}

/// 从本机账号库导出 `PACE_ACCOUNTS`：解密密码 + 本机设备 ID + 校区中心。
/// 输出含明文密码，只写 stdout，用来直接管道给 `gh secret set`。
fn do_export_ci() {
    use pace_forge::api::{accounts, client::DeviceProfile, session};
    let book = accounts::load();
    let mut list = Vec::new();
    for a in &book.accounts {
        if a.password_enc.is_empty() {
            eprintln!("跳过 {}：没存密码", a.id);
            continue;
        }
        let pw = match accounts::open_password(&a.password_enc) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("跳过 {}：密码解不开（{e}）", a.id);
                continue;
            }
        };
        let mut item = serde_json::json!({
            "username": a.username,
            "password": pw,
            "device_id": DeviceProfile::local_device_id(&a.username),
        });
        if let Some((lat, lng)) = session::load_home(&a.uid) {
            item["home"] = serde_json::json!([lat, lng]);
        }
        list.push(item);
    }
    let mut out = serde_json::json!({ "accounts": list });
    if let Some((lat, lng)) = session::load_home("*") {
        out["home"] = serde_json::json!([lat, lng]);
    }
    eprintln!("导出 {} 个账号（stdout 含明文密码，别贴到任何地方）", list.len());
    println!("{out}");
}

fn local_check(dist_km: f64, dur_min: f64) {
    use pace_forge::track::{
        buckets::{make_jitter, speed_buckets},
        detect::{inspect, PointRule},
        simulate, KinParams, SimParams, StadiumSpec,
    };

    let total_m = dist_km * 1000.0;
    let total_sec = (dur_min * 60.0).round() as i64;
    let start_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    println!("离线体检：{dist_km:.1} km / {dur_min:.0} 分钟");
    println!("（不登录、不发请求，只验证本地合成数据）\n");

    // 用操场轨迹，几何确定，便于看点位间距
    let spec = StadiumSpec::new(30.5, 104.0).with_lane(1);
    let lap = pace_forge::track::lap_length(1);
    let laps = (total_m / lap).max(0.5);
    let xy = pace_forge::track::stadium_track(&spec, laps, 5.0);
    println!("  轨迹点      {}", xy.len());
    println!("  绕圈        {laps:.2} 圈（单圈 {lap:.1} m）");

    // 时间戳
    let (times, _sp, real_len) = pace_forge::track::retime(
        &xy, total_sec, start_ms, start_ms as u64,
        &KinParams::default(), 1.0, 8.0,
    );
    let pts3: Vec<(f64, f64, i64)> = xy.iter().zip(&times)
        .map(|((a, b), t)| (*a, *b, *t)).collect();

    // 分桶
    let sb = speed_buckets(&pts3, start_ms, total_sec);
    // 这里原来还造了一份 step_buckets(..., 180, &jit) 赋给 stb 却从没用过
    // （clippy 一直报 unused variable）——分桶由 simulate() 产出。
    let _ = make_jitter(sb.len().max(1), 1.0, start_ms as u64);

    // 合成
    let sp = SimParams {
        total_distance_m: real_len as f64,
        total_sec,
        start_ms,
        seed: start_ms as u64,
        ..Default::default()
    };
    let sim = match simulate(&pts3, &sp) {
        Ok(s) => s,
        Err(e) => { eprintln!("合成失败: {e}"); return; }
    };

    println!("  实际里程    {:.1} m", real_len);
    println!("  总步数      {}", sim.total_steps);
    println!("  平均步频    {:.1} 步/分", sim.avg_cadence);

    // 反检测体检
    let rep = inspect(&xy, &sb, PointRule::default());
    println!("\n=== 点位体检（模拟 7.4.01 本地规则）===");
    println!("  {}", rep.summary());
    for line in rep.lines().iter().take(8) {
        println!("  · {line}");
    }
    println!(
        "\n  判定: {}",
        if rep.blocked() {
            "✗ 会被判异常"
        } else {
            "✓ 通过"
        }
    );

    // 分桶统计
    let caps: Vec<f64> = sb.iter().map(|b| b.distance).collect();
    if !caps.is_empty() {
        let mx = caps.iter().cloned().fold(f64::MIN, f64::max);
        let mn = caps.iter().cloned().fold(f64::MAX, f64::min);
        let avg = caps.iter().sum::<f64>() / caps.len() as f64;
        println!("  分桶        {} 个，{:.1}~{:.1} m（均值 {:.1}）", caps.len(), mn, mx, avg);
    }
}

/// 用本地保存的密码重新登录，刷新 token。
///
/// 密码在 accounts.json 里是加密存的，这里解出来直接用，不用重新输入。
fn do_relogin() {
    use pace_forge::api::{accounts, client, login as login_api, session};

    let mut book = accounts::load();
    let acc = match book.accounts.first().cloned() {
        Some(a) => a,
        None => {
            eprintln!("本地没有账号记录");
            std::process::exit(1);
        }
    };
    if acc.password_enc.is_empty() {
        eprintln!("账号 {} 没存密码，需要重新输入", acc.username);
        std::process::exit(1);
    }
    let password = match accounts::open_password(&acc.password_enc) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("密码解密失败: {e}");
            std::process::exit(1);
        }
    };

    let mut c = client::Client::new(client::DeviceProfile::default());
    match login_api::login(&mut c, &acc.username, &password) {
        Ok(login_api::LoginOutcome::Ok(prof)) => {
            let creds = &c.creds;
            let sess = session::Session {
                uid: creds.uid.clone().unwrap_or_default(),
                token: creds.token.clone().unwrap_or_default(),
                unid: creds.unid.clone().unwrap_or_default(),
                username: acc.username.clone(),
                name: prof.name.clone(),
                campus_name: prof.campus_name.clone(),
                depart: prof.depart.clone(),
                grade_class: prof.grade_class.clone(),
                device_id: String::new(),
            };
            if let Err(e) = session::save_session(&sess) {
                eprintln!("session 写入失败: {e}");
            }
            if let Some(a) = book.get_mut(&acc.id) {
                a.uid = sess.uid.clone();
                a.token = sess.token.clone();
                a.unid = sess.unid.clone();
            }
            let _ = accounts::save(&book);
            println!("✅ 重新登录成功：{} ({})", sess.name, acc.username);
        }
        Ok(login_api::LoginOutcome::NeedCaptcha { message, .. }) => {
            eprintln!("需要过滑动验证码（{message}），请在网页里登录一次");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("登录失败: {e}");
            std::process::exit(1);
        }
    }
}

/// 通知服务端注销当前账号，成功后清掉本地 token。
///
/// 只删本地 session 不等于退出登录——服务端的 token 仍然有效，
/// 换台设备照样能用。这里真的打一次 logout 接口。
fn do_logout() {
    let book = pace_forge::api::accounts::load();
    let acc = match book.accounts.first() {
        Some(a) => a.clone(),
        None => {
            eprintln!("本地没有账号记录，无需注销");
            return;
        }
    };
    let mut client =
        pace_forge::api::client::Client::new(pace_forge::api::client::DeviceProfile::default())
            .with_credentials(pace_forge::api::client::Credentials {
                token: Some(acc.token.clone()),
                uid: Some(acc.uid.clone()),
                unid: Some(acc.unid.clone()),
            });

    match pace_forge::api::login::logout(&mut client) {
        Ok(true) => println!("✅ 服务端已注销 {}", acc.username),
        Ok(false) => println!("服务端没有返回明确的注销结果，可能是 token 已失效"),
        Err(e) => {
            eprintln!("注销请求失败: {e}");
            std::process::exit(1);
        }
    }

    // 本地也要清，否则下次还会带着这枚已经作废的 token 去请求
    if let Err(e) = pace_forge::api::accounts::clear_tokens() {
        eprintln!("本地 token 清理失败: {e}");
    } else {
        println!("本地 token 已清除");
    }
}

fn show_semester() {
    use pace_forge::api::{semester, session};

    let sess = session::load_session();
    if !sess.is_valid() {
        eprintln!("未登录：先执行 `pace-forge web` 在网页里登录一次");
        std::process::exit(1);
    }
    let client = pace_forge::api::client::Client::new(pace_forge::api::client::DeviceProfile::default())
        .with_credentials(pace_forge::api::client::Credentials {
            uid: Some(sess.uid.to_string()),
            token: Some(sess.token.clone()),
            unid: Some(sess.unid.clone()),
        });

    match semester::fetch_summary(&client) {
        Ok(sm) => {
            println!("{}", sm.summary());
            println!("  有效比例 {:.1}%", sm.valid_ratio() * 100.0);
        }
        Err(e) => eprintln!("查学期汇总失败: {e}"),
    }

    match semester::fetch_goal(&client, 1) {
        Ok(g) => {
            if g.target_dis > 0.0 {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                println!(
                    "学期目标 {:.1}km / {} 次，还剩 {} 天",
                    g.target_dis / 1000.0,
                    g.target_count,
                    g.days_left(now)
                );
                if !g.in_term(now) {
                    println!("⚠ 当前不在学期内");
                }
            } else {
                println!("· 未取到学期目标");
            }
        }
        Err(e) => eprintln!("查学期目标失败: {e}"),
    }
}

fn do_run(dist_km: f64, dur_min: f64, yes: bool) {
    run_with_start(dist_km, dur_min, None, yes);
}

/// 通用提交：`start_at` 为 `None` 就是现在开跑。
fn run_with_start(dist_km: f64, dur_min: f64, start_at: Option<i64>, yes: bool) {
    use pace_forge::api::{flow, session};

    let sess = session::load_session();
    if !sess.is_valid() {
        eprintln!("未登录：先执行 `pace-forge web` 在网页里登录一次");
        std::process::exit(1);
    }
    let client = pace_forge::api::client::Client::new(pace_forge::api::client::DeviceProfile::default())
        .with_credentials(pace_forge::api::client::Credentials {
            uid: Some(sess.uid.to_string()),
            token: Some(sess.token.clone()),
            unid: Some(sess.unid.clone()),
        });

    // 中心点同样必须从真实围栏反推，不能用默认的 (0,0)。
    let (clat, clng) = match pace_forge::api::fence::fetch_fences(&client) {
        Ok(fs) if !fs.is_empty() && !fs[0].ring.is_empty() => {
            let n = fs[0].ring.len() as f64;
            let t = fs[0]
                .ring
                .iter()
                .fold((0.0, 0.0), |a, q| (a.0 + q.0, a.1 + q.1));
            (t.0 / n, t.1 / n)
        }
        _ => {
            eprintln!("拿不到围栏中心点，无法规划轨迹");
            std::process::exit(1);
        }
    };

    let p = flow::RunParams {
        center_lat: clat,
        center_lng: clng,
        distance_m: dist_km * 1000.0,
        duration_sec: (dur_min * 60.0).round() as i64,
        start_at_ms: start_at,
        custom_path: None,
        // 户外跑没有服务端时间窗口；记分跑(runMode=1)有，脚本跑容易被 11016 挡掉
        run_mode: std::env::var("PACE_RUN_MODE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2),
        sample_step_m: std::env::var("PACE_STEP_M")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5.0),
        ..Default::default()
    };

    println!("刷跑：{:.1} km / {:.0} 分钟\n", dist_km, dur_min);
    let mut log = |s: &str| println!("{s}");
    let mut confirm = |o: &flow::RunOutcome| -> bool {
        println!();
        println!("=== 待提交 ===");
        println!("  轨迹点   {}", o.track.len());
        println!("  里程     {:.1} m", o.actual_length_m);
        println!("  步数     {}", o.total_steps);
        println!("  步频     {:.1} 步/分", o.avg_cadence);
        println!("  自检     {}", o.report.summary());
        if yes {
            println!("  （-y 已跳过确认）");
            return true;
        }
        print!("\n确认提交？[y/N] ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        matches!(line.trim(), "y" | "Y" | "yes")
    };

    match flow::run_full(&client, &p, &mut log, &mut confirm) {
        Ok(o) => {
            if o.submitted {
                println!("\n✅ 提交成功，记录 id={:?}", o.record_id);
                println!("   {} 秒内不要再跑", flow::SILENCE_SEC);
            } else {
                println!("\n已取消");
            }
        }
        Err(e) => {
            eprintln!("\n失败: {e}");
            std::process::exit(1);
        }
    }
}

fn stadium_probe(laps: f64, lane: u8) {
    use pace_forge::track::{arc_and_curvature, lap_length, path_length, stadium_track, StadiumSpec};

    let spec = StadiumSpec::new(30.5, 104.0).with_lane(lane);
    let pts = stadium_track(&spec, laps, 5.0);
    let len = path_length(&pts);
    let (_, k) = arc_and_curvature(&pts);
    let max_k = k.iter().cloned().fold(0.0f64, f64::max);

    println!("操场第 {lane} 道");
    println!("  单圈      {:.2} m", lap_length(lane));
    println!("  {laps:.1} 圈      {:.1} m", len);
    println!("  采样点    {}", pts.len());
    println!("  最大曲率  {max_k:.5} /m（理论 {:.5}）", 1.0 / (36.5 + 0.3 + 1.22 * (lane as f64 - 1.0)));
    println!();
    println!("建议：{:.0}m 跑 {:.0} 秒（配速 {:.2} 分/km）", len, (len / 2.78).round(), 1000.0 / 2.78 / 60.0);
}


// ---------------------------------------------------------------------------
// 补跑：指定时间
// ---------------------------------------------------------------------------

fn show_plan(st: pace_forge::api::start_time::StartTime, dur_min: f64) {
    let dur_s = (dur_min * 60.0).round() as i64;
    println!("开始 {}", pace_forge::api::start_time::format_ms(
        st.ms, pace_forge::api::start_time::default_tz_offset_min()));
    println!("结束 {}", pace_forge::api::start_time::format_ms(
        st.stop_ms(dur_s), pace_forge::api::start_time::default_tz_offset_min()));
    if st.clamped {
        println!("注意：原时刻会让结束时间落在未来，已自动提前");
    }
}

fn do_at(hhmm: &str, days: i64, dist_km: f64, dur_min: f64, yes: bool) {
    use pace_forge::api::start_time as st_mod;
    let (h, m) = match st_mod::parse_hhmm(hhmm) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let now = pace_forge::crypto::envelope::now_ms() as i64;
    let dur_s = (dur_min * 60.0).round() as i64;
    let st = match st_mod::specified(now, days, h, m, dur_s) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    show_plan(st, dur_min);
    run_with_start(dist_km, dur_min, Some(st.ms), yes);
}

fn do_ago(mins: i64, dist_km: f64, dur_min: f64, yes: bool) {
    use pace_forge::api::start_time as st_mod;
    let now = pace_forge::crypto::envelope::now_ms() as i64;
    let dur_s = (dur_min * 60.0).round() as i64;
    let st = match st_mod::ago(now, mins, dur_s) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    show_plan(st, dur_min);
    run_with_start(dist_km, dur_min, Some(st.ms), yes);
}

fn do_rand(days: i64, dist_km: f64, dur_min: f64, yes: bool) {
    use pace_forge::api::start_time as st_mod;
    use rand::Rng;
    let now = pace_forge::crypto::envelope::now_ms() as i64;
    let dur_s = (dur_min * 60.0).round() as i64;
    let r: f64 = rand::thread_rng().gen();
    let st = match st_mod::random_in_day(now, days, dur_s, r) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    show_plan(st, dur_min);
    run_with_start(dist_km, dur_min, Some(st.ms), yes);
}

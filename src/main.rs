//! disk-steward: a std-only host disk steward (Bobby 2026-10-25).
//!
//! One run = measure free space, walk the DECLARED consumers, sweep by policy (smallest blast radius first:
//! logs, then scratch rust targets, then age-limited captures, then nix gc), write a report table, and send
//! at most one contentless ntfy pointer per incident (free space under the floor). Nothing outside a declared
//! consumer is ever deleted; undeclared big directories are only reported. Deletion needs BOTH `mode=live`
//! and `enabled=true` on the consumer; the default is a read-only dry run.
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

const GB: u64 = 1024 * 1024 * 1024;
const MB: u64 = 1024 * 1024;

#[derive(Debug, Clone, Default)]
struct Consumer {
    name: String,
    kv: BTreeMap<String, String>,
}

impl Consumer {
    fn s(&self, k: &str) -> Option<&str> {
        self.kv.get(k).map(|v| v.as_str())
    }
    fn n(&self, k: &str, d: u64) -> u64 {
        self.s(k).and_then(|v| v.parse().ok()).unwrap_or(d)
    }
    fn list(&self, k: &str) -> Vec<String> {
        self.s(k)
            .map(|v| v.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
            .unwrap_or_default()
    }
    fn enabled(&self) -> bool {
        self.s("enabled") == Some("true")
    }
}

#[derive(Debug, Default)]
struct Config {
    global: BTreeMap<String, String>,
    consumers: Vec<Consumer>,
}

impl Config {
    fn g(&self, k: &str) -> Option<&str> {
        self.global.get(k).map(|v| v.as_str())
    }
    fn gn(&self, k: &str, d: u64) -> u64 {
        self.g(k).and_then(|v| v.parse().ok()).unwrap_or(d)
    }
    fn live(&self) -> bool {
        self.g("mode") == Some("live")
    }
}

/// `key=value` lines; `[consumer NAME]` opens a consumer; `#` comments. Unknown keys are kept, never fatal.
fn parse_config(text: &str) -> Config {
    let mut c = Config::default();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("[consumer ").and_then(|r| r.strip_suffix(']')) {
            c.consumers.push(Consumer { name: rest.trim().to_string(), kv: BTreeMap::new() });
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let (k, v) = (k.trim().to_string(), v.trim().to_string());
            match c.consumers.last_mut() {
                Some(cons) => {
                    cons.kv.insert(k, v);
                }
                None => {
                    c.global.insert(k, v);
                }
            }
        }
    }
    c
}

/// Disk usage of a tree in bytes (allocated blocks), and the newest mtime inside it. Symlinks are not followed.
fn tree_stats(p: &Path) -> (u64, SystemTime) {
    let mut bytes = 0u64;
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let md = match fs::symlink_metadata(&d) {
            Ok(m) => m,
            Err(_) => continue,
        };
        bytes += md.blocks() * 512;
        if let Ok(t) = md.modified() {
            if t > newest {
                newest = t;
            }
        }
        if md.is_dir() {
            if let Ok(rd) = fs::read_dir(&d) {
                for e in rd.flatten() {
                    stack.push(e.path());
                }
            }
        }
    }
    (bytes, newest)
}

fn is_rust_target(p: &Path) -> bool {
    p.join(".rustc_info.json").is_file() || (p.join("CACHEDIR.TAG").is_file() && (p.join("debug").is_dir() || p.join("release").is_dir()))
}

/// Cargo target dirs under `root` (depth-limited; a found target is not descended into). Never returns a dir holding a `.git`.
fn find_targets(root: &Path, depth: u32, exclude: &[String], out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let rd = match fs::read_dir(root) {
        Ok(r) => r,
        Err(_) => return,
    };
    for e in rd.flatten() {
        let p = e.path();
        let ft = match e.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if name == ".git" || name == "node_modules" || exclude.iter().any(|x| p.starts_with(x)) {
            continue;
        }
        if is_rust_target(&p) {
            if !p.join(".git").exists() {
                out.push(p);
            }
        } else {
            find_targets(&p, depth - 1, exclude, out);
        }
    }
}

fn has_open_files(p: &Path, lsof: &str) -> bool {
    // lsof -t +D lists pids with files open under the dir. If lsof is missing or fails we say "open": the safe answer.
    match Command::new(lsof).arg("-t").arg("+D").arg(p).output() {
        Ok(o) => !o.stdout.is_empty() || !(o.status.success() || o.status.code() == Some(1)),
        Err(_) => true,
    }
}

fn free_bytes(volume: &str) -> Option<u64> {
    let o = Command::new("df").arg("-k").arg(volume).output().ok()?;
    let t = String::from_utf8_lossy(&o.stdout).to_string();
    let line = t.lines().nth(1)?;
    let f: Vec<&str> = line.split_whitespace().collect();
    f.get(3)?.parse::<u64>().ok().map(|k| k * 1024)
}

fn gb(b: u64) -> String {
    format!("{:.1}", b as f64 / GB as f64)
}

struct Row {
    name: String,
    kind: String,
    enabled: bool,
    size: u64,
    budget: u64,
    action: String,
}

/// Rotate `path` copy-truncate style (the writer keeps its fd). `.1` is newest, `.keep` is dropped.
fn rotate_log(path: &Path, keep: u64, live: bool) -> String {
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if !live {
        return format!("would rotate {} MB", size / MB);
    }
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let sib = |i: u64| path.with_file_name(format!("{}.{}", name, i));
    let _ = fs::remove_file(sib(keep));
    for i in (1..keep).rev() {
        let _ = fs::rename(sib(i), sib(i + 1));
    }
    // A giant file (>1 GB) is truncated without a copy: copying it would need as much free space as it holds.
    if size > GB {
        return match fs::OpenOptions::new().write(true).truncate(true).open(path) {
            Ok(_) => format!("truncated {} MB (too big to keep a copy)", size / MB),
            Err(e) => format!("truncate failed: {}", e),
        };
    }
    if fs::copy(path, sib(1)).is_err() {
        return "copy failed, not truncated".to_string();
    }
    match fs::OpenOptions::new().write(true).truncate(true).open(path) {
        Ok(_) => format!("rotated {} MB", size / MB),
        Err(e) => format!("truncate failed: {}", e),
    }
}

struct Run<'a> {
    cfg: &'a Config,
    free: u64,
    floor: u64,
    now: SystemTime,
}

fn sweep_targets(r: &Run, c: &Consumer) -> Row {
    let roots = c.list("roots");
    let exclude = c.list("exclude");
    let budget = c.n("budget_gb", 60) * GB;
    let min_age = Duration::from_secs(c.n("min_age_hours", 24) * 3600);
    let lsof = r.cfg.g("lsof").unwrap_or("lsof").to_string();
    let mut found = Vec::new();
    for root in &roots {
        find_targets(Path::new(root), c.n("depth", 5) as u32, &exclude, &mut found);
    }
    let mut items: Vec<(PathBuf, u64, SystemTime)> = found.into_iter().map(|p| { let (b, t) = tree_stats(&p); (p, b, t) }).collect();
    let total: u64 = items.iter().map(|i| i.1).sum();
    let over = total > budget || r.free < r.floor;
    let mut action = format!("{} targets", items.len());
    if over {
        items.sort_by_key(|i| i.2); // oldest first
        let mut cur = total;
        let mut done = 0;
        let mut freed = 0u64;
        for (p, b, newest) in &items {
            if cur <= budget && r.free + freed >= r.floor {
                break;
            }
            let age = r.now.duration_since(*newest).unwrap_or(Duration::ZERO);
            if age < min_age || has_open_files(p, &lsof) {
                continue;
            }
            if r.cfg.live() && c.enabled() {
                if fs::remove_dir_all(p).is_err() {
                    continue;
                }
            }
            cur -= b;
            freed += b;
            done += 1;
        }
        let verb = if r.cfg.live() && c.enabled() { "deleted" } else { "would delete" };
        action = format!("{}; {} {} ({} GB)", action, verb, done, gb(freed));
    }
    Row { name: c.name.clone(), kind: "rust-target".into(), enabled: c.enabled(), size: total, budget, action }
}

fn sweep_logs(r: &Run, c: &Consumer) -> Row {
    let rotate = c.n("rotate_mb", 100) * MB;
    let keep = c.n("keep", 5).max(1);
    let (mut size, mut notes) = (0u64, Vec::new());
    for f in c.list("files") {
        let p = Path::new(&f);
        let len = match fs::metadata(p) { Ok(m) => m.len(), Err(_) => continue };
        size += len;
        if len > rotate {
            notes.push(format!("{}: {}", p.file_name().unwrap().to_string_lossy(), rotate_log(p, keep, r.cfg.live() && c.enabled())));
        }
    }
    let action = if notes.is_empty() { "ok".to_string() } else { notes.join("; ") };
    Row { name: c.name.clone(), kind: "logs".into(), enabled: c.enabled(), size, budget: rotate, action }
}

fn sweep_age(r: &Run, c: &Consumer) -> Row {
    let max_age = Duration::from_secs(c.n("max_age_days", 7) * 86400);
    let live = r.cfg.live() && c.enabled();
    let (mut size, mut old, mut oldb, mut gone) = (0u64, 0u64, 0u64, 0u64);
    let mut stack = vec![PathBuf::from(c.s("path").unwrap_or(""))];
    while let Some(d) = stack.pop() {
        let rd = match fs::read_dir(&d) { Ok(r) => r, Err(_) => continue };
        for e in rd.flatten() {
            let md = match fs::symlink_metadata(e.path()) { Ok(m) => m, Err(_) => continue };
            if md.is_dir() { stack.push(e.path()); continue; }
            let b = md.blocks() * 512;
            size += b;
            let age = md.modified().ok().and_then(|t| r.now.duration_since(t).ok()).unwrap_or(Duration::ZERO);
            if age > max_age {
                old += 1;
                oldb += b;
                if live && fs::remove_file(e.path()).is_ok() { gone += 1; }
            }
        }
    }
    let verb = if live { format!("deleted {}", gone) } else { "would delete".to_string() };
    Row { name: c.name.clone(), kind: "age-files".into(), enabled: c.enabled(), size, budget: c.n("budget_gb", 0) * GB, action: format!("{} files over {}d ({} GB): {}", old, max_age.as_secs() / 86400, gb(oldb), verb) }
}

fn watch(c: &Consumer) -> Row {
    let (size, _) = tree_stats(Path::new(c.s("path").unwrap_or("")));
    let budget = c.n("budget_gb", 0) * GB;
    let action = if budget > 0 && size > budget { "OVER BUDGET (report only, never deleted)" } else { "report only" };
    Row { name: c.name.clone(), kind: "watch".into(), enabled: false, size, budget, action: action.into() }
}

fn nix_gc(r: &Run, c: &Consumer, free_now: u64) -> Row {
    let days = c.n("keep_days", 14);
    let live = r.cfg.live() && c.enabled();
    let need = free_now < r.floor;
    let action = if !need { "free space above floor, not run".to_string() } else if !live { format!("would run nix-collect-garbage --delete-older-than {}d", days) } else {
        let bin = c.s("bin").unwrap_or("nix-collect-garbage");
        match Command::new(bin).arg("--delete-older-than").arg(format!("{}d", days)).output() {
            Ok(o) if o.status.success() => format!("ran nix-collect-garbage --delete-older-than {}d", days),
            Ok(o) => format!("nix-collect-garbage exit {:?}", o.status.code()),
            Err(e) => format!("nix-collect-garbage not run: {}", e),
        }
    };
    Row { name: c.name.clone(), kind: "nix-gc".into(), enabled: c.enabled(), size: 0, budget: 0, action }
}

/// Big top-level dirs under `scan_roots` that no declared consumer covers: reported, never touched.
fn undeclared(cfg: &Config, threshold: u64) -> Vec<(String, u64)> {
    let mut declared: Vec<String> = Vec::new();
    for c in &cfg.consumers {
        declared.extend(c.list("roots"));
        declared.extend(c.list("files"));
        if let Some(p) = c.s("path") { declared.push(p.to_string()); }
    }
    let mut out = Vec::new();
    for root in cfg.g("scan_roots").unwrap_or("").split(',').map(|x| x.trim()).filter(|x| !x.is_empty()) {
        if let Ok(rd) = fs::read_dir(root) {
            for e in rd.flatten() {
                let p = e.path();
                if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) { continue; }
                if declared.iter().any(|d| Path::new(d).starts_with(&p) || p.starts_with(d)) { continue; }
                let (b, _) = tree_stats(&p);
                if b >= threshold { out.push((p.to_string_lossy().to_string(), b)); }
            }
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}

fn table(free: u64, floor: u64, live: bool, rows: &[Row], und: &[(String, u64)]) -> String {
    let mut t = format!("disk-steward {} free {} GB floor {} GB\n", if live { "LIVE" } else { "DRY-RUN" }, gb(free), gb(floor));
    t.push_str("consumer | kind | enabled | size GB | budget GB | action\n");
    for r in rows {
        t.push_str(&format!("{} | {} | {} | {} | {} | {}\n", r.name, r.kind, r.enabled, gb(r.size), gb(r.budget), r.action));
    }
    for (p, b) in und {
        t.push_str(&format!("UNDECLARED (report only) | {} | {} GB\n", p, gb(*b)));
    }
    t
}

fn json_esc(s: &str) -> String {
    s.chars().map(|c| match c { '"' => "\\\"".to_string(), '\\' => "\\\\".to_string(), c if (c as u32) < 32 => " ".to_string(), c => c.to_string() }).collect()
}

fn json(free: u64, floor: u64, live: bool, rows: &[Row], und: &[(String, u64)], now: u64) -> String {
    let r: Vec<String> = rows.iter().map(|r| format!("{{\"name\":\"{}\",\"kind\":\"{}\",\"enabled\":{},\"size_bytes\":{},\"budget_bytes\":{},\"action\":\"{}\"}}", json_esc(&r.name), r.kind, r.enabled, r.size, r.budget, json_esc(&r.action))).collect();
    let u: Vec<String> = und.iter().map(|(p, b)| format!("{{\"path\":\"{}\",\"size_bytes\":{}}}", json_esc(p), b)).collect();
    format!("{{\"at\":{},\"mode\":\"{}\",\"free_bytes\":{},\"floor_bytes\":{},\"below_floor\":{},\"consumers\":[{}],\"undeclared\":[{}]}}\n", now, if live { "live" } else { "dry-run" }, free, floor, free < floor, r.join(","), u.join(","))
}

fn run_cfg(cfg: &Config) -> (String, bool) {
    let now = SystemTime::now();
    let vol = cfg.g("volume").unwrap_or("/System/Volumes/Data");
    let floor = cfg.gn("floor_gb", 150) * GB;
    let free0 = free_bytes(vol).unwrap_or(u64::MAX);
    let r = Run { cfg, free: free0, floor, now };
    let order = ["logs", "rust-target", "age-files", "watch", "nix-gc"];
    let mut rows = Vec::new();
    for kind in order {
        for c in cfg.consumers.iter().filter(|c| c.s("kind") == Some(kind)) {
            rows.push(match kind {
                "logs" => sweep_logs(&r, c),
                "rust-target" => sweep_targets(&r, c),
                "age-files" => sweep_age(&r, c),
                "watch" => watch(c),
                _ => { let f = free_bytes(vol).unwrap_or(free0); nix_gc(&r, c, f) }
            });
        }
    }
    let und = undeclared(cfg, cfg.gn("undeclared_gb", 5) * GB);
    let free = free_bytes(vol).unwrap_or(free0);
    let epoch = now.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let sd = PathBuf::from(cfg.g("state_dir").unwrap_or("."));
    let _ = fs::create_dir_all(&sd);
    let t = table(free, floor, cfg.live(), &rows, &und);
    let _ = fs::write(sd.join("report.txt"), &t);
    let _ = fs::write(sd.join("report.json"), json(free, floor, cfg.live(), &rows, &und, epoch));
    // One contentless pointer per incident: sent on the first run below the floor, re-armed when free space recovers.
    let flag = sd.join("incident");
    if free < floor {
        if !flag.exists() {
            if let Some(n) = cfg.g("ntfy") {
                let ok = Command::new(n).args(["push", "-t", "DATA home stalled", "host disk below the floor, see the fleet map"]).output().map(|o| o.status.success()).unwrap_or(false);
                if ok { let _ = fs::write(&flag, ""); }
            }
        }
    } else {
        let _ = fs::remove_file(&flag);
    }
    // Report to chitin (best effort: the topic schema is the router's, a refusal is logged not fatal).
    if let (Some(ch), Some(who)) = (cfg.g("chitin"), cfg.g("chitin_as")) {
        let subj = format!("free {} GB floor {} GB {}", gb(free), gb(floor), if cfg.live() { "live" } else { "dry-run" });
        let _ = Command::new(ch).args(["publish", "--as", who, "disk.report", "report", &subj]).output();
    }
    (t, free < floor)
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let path = match a.get(1) {
        Some(p) => p.clone(),
        None => { eprintln!("usage: disk-steward CONFIG"); std::process::exit(64); }
    };
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => { eprintln!("disk-steward: cannot read {}: {}", path, e); std::process::exit(66); }
    };
    let cfg = parse_config(&text);
    let (t, _) = run_cfg(&cfg);
    print!("{}", t);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ds-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }
    fn mk_target(d: &Path, name: &str, kb: usize) -> PathBuf {
        let t = d.join(name);
        fs::create_dir_all(t.join("debug")).unwrap();
        fs::write(t.join(".rustc_info.json"), "{}").unwrap();
        fs::write(t.join("debug/blob"), vec![1u8; kb * 1024]).unwrap();
        t
    }
    fn age(p: &Path, hours: u64) {
        let t = SystemTime::now() - Duration::from_secs(hours * 3600);
        let f = fs::File::options().write(true).open(p).unwrap();
        f.set_modified(t).unwrap();
    }
    fn ageall(p: &Path, hours: u64) {
        let mut st = vec![p.to_path_buf()];
        while let Some(d) = st.pop() {
            if d.is_dir() { for e in fs::read_dir(&d).unwrap().flatten() { st.push(e.path()); } let f = fs::File::open(&d).unwrap(); let _ = f.set_modified(SystemTime::now() - Duration::from_secs(hours * 3600)); } else { age(&d, hours); }
        }
    }
    fn cfg_for(mode: &str, enabled: &str, root: &Path, budget_gb: u64, floor_gb: u64) -> Config {
        parse_config(&format!("mode={}\nfloor_gb={}\nlsof=true\nstate_dir={}/state\n[consumer rt]\nkind=rust-target\nenabled={}\nroots={}\nbudget_gb={}\nmin_age_hours=24\n", mode, floor_gb, root.display(), enabled, root.display(), budget_gb))
    }

    #[test]
    fn parses_global_and_consumers() {
        let c = parse_config("# c\nmode=live\nfloor_gb=150\n[consumer a]\nkind=logs\nfiles=/x, /y\n[consumer b]\nkind=watch\n");
        assert!(c.live());
        assert_eq!(c.gn("floor_gb", 0), 150);
        assert_eq!(c.consumers.len(), 2);
        assert_eq!(c.consumers[0].list("files"), vec!["/x", "/y"]);
    }

    #[test]
    fn dry_run_deletes_nothing_even_when_enabled() {
        let d = tmp("dry");
        let t = mk_target(&d, "old", 64);
        ageall(&t, 100);
        let c = cfg_for("dry-run", "true", &d, 0, 0);
        let (out, _) = run_cfg(&c);
        assert!(t.exists(), "dry run must not delete");
        assert!(out.contains("would delete 1"), "{}", out);
    }

    #[test]
    fn live_but_consumer_not_enabled_deletes_nothing() {
        let d = tmp("noen");
        let t = mk_target(&d, "old", 64);
        ageall(&t, 100);
        let c = cfg_for("live", "false", &d, 0, 0);
        let _ = run_cfg(&c);
        assert!(t.exists());
    }

    #[test]
    fn live_enabled_deletes_old_but_skips_recent_and_git_checkouts() {
        let d = tmp("live");
        let old = mk_target(&d, "old", 64);
        ageall(&old, 100);
        let fresh = mk_target(&d, "fresh", 64); // mtime now: < 24h
        let git = mk_target(&d, "gitty", 64);
        fs::create_dir_all(git.join(".git")).unwrap();
        ageall(&git, 100);
        let c = cfg_for("live", "true", &d, 0, 0);
        let _ = run_cfg(&c);
        assert!(!old.exists(), "old target should go");
        assert!(fresh.exists(), "recent target must stay");
        assert!(git.exists(), "a dir holding .git must stay");
    }

    #[test]
    fn open_files_block_deletion() {
        let d = tmp("open");
        let t = mk_target(&d, "old", 64);
        ageall(&t, 100);
        let mut c = cfg_for("live", "true", &d, 0, 0);
        c.global.insert("lsof".into(), "echo".into()); // prints args, non-empty stdout = "open"
        let _ = run_cfg(&c);
        assert!(t.exists());
    }

    #[test]
    fn under_budget_and_above_floor_touches_nothing() {
        let d = tmp("budget");
        let t = mk_target(&d, "old", 64);
        ageall(&t, 100);
        let c = cfg_for("live", "true", &d, 60, 0);
        let _ = run_cfg(&c);
        assert!(t.exists());
    }

    #[test]
    fn log_rotation_keeps_n_and_truncates() {
        let d = tmp("log");
        let f = d.join("out.log");
        fs::write(&f, vec![b'a'; 2 * 1024]).unwrap();
        for _ in 0..4 {
            let n = rotate_log(&f, 3, true);
            assert!(n.starts_with("rotated"), "{}", n);
            fs::write(&f, vec![b'b'; 2 * 1024]).unwrap();
        }
        assert!(d.join("out.log.1").exists() && d.join("out.log.2").exists() && d.join("out.log.3").exists());
        assert!(!d.join("out.log.4").exists(), "keep=3 means no .4");
    }

    #[test]
    fn age_files_report_only_unless_enabled_and_live() {
        let d = tmp("age");
        let f = d.join("a.json");
        fs::write(&f, "x").unwrap();
        age(&f, 24 * 30);
        let c = parse_config(&format!("mode=live\nstate_dir={}/st\n[consumer cap]\nkind=age-files\nenabled=false\npath={}\nmax_age_days=7\n", d.display(), d.display()));
        let _ = run_cfg(&c);
        assert!(f.exists());
        let c = parse_config(&format!("mode=live\nstate_dir={}/st\n[consumer cap]\nkind=age-files\nenabled=true\npath={}\nmax_age_days=7\n", d.display(), d.display()));
        let _ = run_cfg(&c);
        assert!(!f.exists());
    }

    #[test]
    fn undeclared_dirs_reported_not_deleted() {
        let d = tmp("und");
        let big = d.join("big");
        fs::create_dir_all(&big).unwrap();
        fs::write(big.join("f"), vec![1u8; 8192]).unwrap();
        let c = parse_config(&format!("mode=live\nstate_dir={}/st\nscan_roots={}\nundeclared_gb=0\n", d.display(), d.display()));
        let (t, _) = run_cfg(&c);
        assert!(t.contains("UNDECLARED"), "{}", t);
        assert!(big.join("f").exists());
    }
}

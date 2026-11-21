use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

struct Config {
    base_dir: PathBuf,
    sort_by_size: bool,
    dry_run: bool,
    no_size: bool,
    workers: usize,
}

struct Item {
    project: String,
    path: PathBuf,
    size: Option<u64>,
    age_days: Option<u64>,
}

fn default_base_dir() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn default_workers() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

fn parse_args() -> Result<Config, i32> {
    let mut base_dir = None;
    let mut sort_by_size = false;
    let mut dry_run = false;
    let mut no_size = false;
    let mut workers = default_workers();

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!(
                    "Usage: nodekilla [TARGET_DIR] [OPTIONS]\n\n\
                     Options:\n  \
                       TARGET_DIR       Directory to scan (default: directory of executable)\n  \
                       -s, --sort-size  Sort results by size descending\n  \
                       -n, --dry-run    Simulate deletion without removing files\n  \
                       --no-size        Skip folder size calculation for fast previews\n  \
                       -j, --workers N  Number of worker threads (default: logical CPUs)\n  \
                       -h, --help       Show this help message"
                );
                return Err(0);
            }
            "--sort-size" | "-s" => sort_by_size = true,
            "--dry-run" | "-n" => dry_run = true,
            "--no-size" => no_size = true,
            "--workers" | "-j" => {
                let Some(val) = args.next() else {
                    eprintln!("Missing value for {arg}");
                    return Err(1);
                };
                match val.parse::<usize>() {
                    Ok(n) if n >= 1 => workers = n,
                    _ => {
                        eprintln!("Invalid worker count: {val} (must be >= 1)");
                        return Err(1);
                    }
                }
            }
            _ if arg.starts_with('-') => {
                eprintln!("Unknown option: {arg} (use --help for usage)");
                return Err(1);
            }
            _ => {
                if base_dir.is_none() {
                    base_dir = Some(PathBuf::from(arg));
                } else {
                    eprintln!("Unexpected extra argument: {arg}");
                    return Err(1);
                }
            }
        }
    }

    if sort_by_size && no_size {
        eprintln!("Cannot combine --sort-size with --no-size");
        return Err(1);
    }

    let base_dir = base_dir.unwrap_or_else(default_base_dir);
    let base_dir = fs::canonicalize(&base_dir).unwrap_or(base_dir);

    Ok(Config {
        base_dir,
        sort_by_size,
        dry_run,
        no_size,
        workers,
    })
}

/// Pure-std bounded work-sharing queue using a single Mutex + Condvar thread pool.
fn parallel_work_queue<T, F>(initial: Vec<T>, workers: usize, visit: F)
where
    T: Send,
    F: Fn(T) -> Vec<T> + Sync,
{
    if initial.is_empty() {
        return;
    }

    struct QueueState<T> {
        tasks: Vec<T>,
        active: usize,
    }

    let queue = Mutex::new(QueueState {
        tasks: initial,
        active: 0,
    });
    let cvar = Condvar::new();
    let num_workers = workers.max(1);

    thread::scope(|s| {
        for _ in 0..num_workers {
            s.spawn(|| {
                loop {
                    let task = {
                        let mut state = queue.lock().unwrap();
                        loop {
                            if let Some(t) = state.tasks.pop() {
                                state.active += 1;
                                break Some(t);
                            }
                            if state.active == 0 {
                                cvar.notify_all();
                                break None;
                            }
                            state = cvar.wait(state).unwrap();
                        }
                    };

                    let Some(task) = task else { break };
                    let next_tasks = visit(task);

                    let mut state = queue.lock().unwrap();
                    let added = !next_tasks.is_empty();
                    state.tasks.extend(next_tasks);
                    state.active -= 1;

                    if added || (state.tasks.is_empty() && state.active == 0) {
                        cvar.notify_all();
                    }
                }
            });
        }
    });
}

fn age_in_days(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let elapsed = SystemTime::now().duration_since(modified).ok()?;
    Some(elapsed.as_secs() / 86_400)
}

/// Phase 1: Discover all `node_modules` folders using one shared worker pool.
fn discover_node_modules(base: &Path, workers: usize) -> Vec<PathBuf> {
    let found = Mutex::new(Vec::<PathBuf>::new());

    parallel_work_queue(vec![base.to_path_buf()], workers, |dir| {
        let mut subdirs = Vec::new();
        let mut local_found = Vec::new();

        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else {
                    continue;
                };
                if ft.is_symlink() || !ft.is_dir() {
                    continue;
                }
                let name = entry.file_name();
                let name_str = name.to_string_lossy();

                if name_str == "node_modules" {
                    local_found.push(entry.path());
                    continue; // Never descend into node_modules
                }
                if name_str.starts_with('.') {
                    continue; // Never descend into hidden directories
                }
                subdirs.push(entry.path());
            }
        }

        if !local_found.is_empty() {
            found.lock().unwrap().extend(local_found);
        }
        subdirs
    });

    found.into_inner().unwrap()
}

/// Phase 2: Size ALL discovered `node_modules` folders using ONE reused worker pool.
fn size_all_parallel(paths: &[PathBuf], workers: usize) -> Vec<u64> {
    let totals: Vec<AtomicU64> = (0..paths.len()).map(|_| AtomicU64::new(0)).collect();
    let initial: Vec<(usize, PathBuf)> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| (i, p.clone()))
        .collect();

    parallel_work_queue(initial, workers, |(idx, dir)| {
        let mut subdirs = Vec::new();
        let mut local_bytes: u64 = 0;

        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else {
                    continue;
                };
                if ft.is_symlink() {
                    continue;
                }
                if ft.is_dir() {
                    subdirs.push((idx, entry.path()));
                } else if let Ok(meta) = entry.metadata() {
                    local_bytes += meta.len();
                }
            }
        }

        if local_bytes > 0 {
            totals[idx].fetch_add(local_bytes, Ordering::Relaxed);
        }
        subdirs
    });

    totals.into_iter().map(AtomicU64::into_inner).collect()
}

fn scan_projects(cfg: &Config) -> (Vec<Item>, Duration, Option<Duration>) {
    let discover_start = Instant::now();
    let nm_paths = discover_node_modules(&cfg.base_dir, cfg.workers);
    let discover_elapsed = discover_start.elapsed();

    if nm_paths.is_empty() {
        return (Vec::new(), discover_elapsed, None);
    }

    let (sizes, size_elapsed) = if cfg.no_size {
        (vec![None; nm_paths.len()], None)
    } else {
        let size_start = Instant::now();
        let raw_sizes = size_all_parallel(&nm_paths, cfg.workers);
        let elapsed = size_start.elapsed();
        (raw_sizes.into_iter().map(Some).collect(), Some(elapsed))
    };

    let mut items: Vec<Item> = nm_paths
        .into_iter()
        .zip(sizes)
        .map(|(path, size)| {
            let project = path
                .strip_prefix(&cfg.base_dir)
                .ok()
                .and_then(|rel| rel.components().next())
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .unwrap_or_else(|| ".".to_string());

            let age_days = age_in_days(&path);

            Item {
                project,
                path,
                size,
                age_days,
            }
        })
        .collect();

    items.sort_by(|a, b| {
        a.project
            .to_lowercase()
            .cmp(&b.project.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });

    (items, discover_elapsed, size_elapsed)
}

fn human_size(num: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut val = num as f64;
    for &unit in &units {
        if val < 1024.0 || unit == "TB" {
            return if unit != "B" {
                format!("{val:.1} {unit}")
            } else {
                format!("{num} B")
            };
        }
        val /= 1024.0;
    }
    format!("{val:.1} TB")
}

fn format_size(size: Option<u64>) -> String {
    match size {
        Some(bytes) => human_size(bytes),
        None => "-".to_string(),
    }
}

fn format_age(days: Option<u64>) -> String {
    match days {
        Some(0) => "today".to_string(),
        Some(d) => format!("{d}d ago"),
        None => "?".to_string(),
    }
}

/// Parse '1 3 5-7' style input with range clamping to prevent huge-range hangs.
fn parse_selection(text: &str, max_num: usize) -> Vec<usize> {
    if max_num == 0 {
        return Vec::new();
    }

    let mut picked = BTreeSet::new();
    for token in text.replace(',', " ").split_whitespace() {
        if let Some((a, b)) = token.split_once('-') {
            if !a.is_empty()
                && !b.is_empty()
                && a.chars().all(|c| c.is_ascii_digit())
                && b.chars().all(|c| c.is_ascii_digit())
            {
                if let (Ok(start), Ok(end)) = (a.parse::<usize>(), b.parse::<usize>()) {
                    let (lo, hi) = if start <= end {
                        (start, end)
                    } else {
                        (end, start)
                    };
                    if hi < 1 || lo > max_num {
                        continue;
                    }
                    let clamped_lo = lo.max(1);
                    let clamped_hi = hi.min(max_num);
                    for n in clamped_lo..=clamped_hi {
                        picked.insert(n);
                    }
                }
            }
        } else if token.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = token.parse::<usize>() {
                if n >= 1 && n <= max_num {
                    picked.insert(n);
                }
            }
        }
    }
    picked.into_iter().collect()
}

fn prompt_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut buf = String::new();
    match io::stdin().read_line(&mut buf) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(buf),
    }
}

fn rel_display(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn is_safe_target(path: &Path, base: &Path) -> bool {
    let is_nm = path
        .file_name()
        .map(|n| n == "node_modules")
        .unwrap_or(false);
    if !is_nm {
        return false;
    }
    match (fs::canonicalize(path), fs::canonicalize(base)) {
        (Ok(canon_path), Ok(canon_base)) => canon_path.starts_with(&canon_base),
        _ => false,
    }
}

fn main() {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(code) => std::process::exit(code),
    };

    println!(
        "\nScanning projects in: {} (workers: {})",
        cfg.base_dir.display(),
        cfg.workers
    );
    if cfg.dry_run {
        println!("(DRY RUN mode: no files will be deleted)");
    }

    let (mut items, discover_elapsed, size_elapsed) = scan_projects(&cfg);

    if items.is_empty() {
        println!(
            "\nNo node_modules folders found (discovery: {:.2}s). Nothing to do. 🎉",
            discover_elapsed.as_secs_f64()
        );
        return;
    }

    if cfg.sort_by_size {
        items.sort_by(|a, b| b.size.unwrap_or(0).cmp(&a.size.unwrap_or(0)));
    }

    match size_elapsed {
        Some(se) => println!(
            "\nFound {} node_modules (discovery: {:.2}s, sizing: {:.2}s):\n",
            items.len(),
            discover_elapsed.as_secs_f64(),
            se.as_secs_f64()
        ),
        None => println!(
            "\nFound {} node_modules (discovery: {:.2}s, sizing: skipped):\n",
            items.len(),
            discover_elapsed.as_secs_f64()
        ),
    }

    for (idx, item) in items.iter().enumerate() {
        let i = idx + 1;
        let rel = rel_display(&item.path, &cfg.base_dir);
        println!(
            "  [{i:>2}] {:<20} {:>10}  {:>7}  ({rel})",
            item.project,
            format_size(item.size),
            format_age(item.age_days)
        );
    }

    if !cfg.no_size {
        let total: u64 = items.iter().filter_map(|i| i.size).sum();
        println!("\n  Total reclaimable: {}", human_size(total));
    }

    println!("\nSelect which to delete:");
    println!("  - numbers separated by space/comma, ranges ok: 1 3 5-7");
    println!("  - 'a' = select ALL");
    println!("  - 'q' = quit without deleting anything");

    let selected: Vec<usize> = loop {
        let input = match prompt_line("\nYour selection: ") {
            Some(line) => line,
            None => {
                println!("\nInterrupted. Nothing deleted.");
                return;
            }
        };

        let choice = input.trim().to_lowercase();
        if matches!(choice.as_str(), "q" | "quit" | "exit" | "") {
            println!("Aborted. Nothing deleted.");
            return;
        }

        let sel = if matches!(choice.as_str(), "a" | "all") {
            (1..=items.len()).collect()
        } else {
            parse_selection(&choice, items.len())
        };

        if !sel.is_empty() {
            break sel;
        }
        println!("  Invalid selection, try again.");
    };

    let to_delete: Vec<&Item> = selected.iter().map(|&i| &items[i - 1]).collect();

    if cfg.no_size {
        println!("\nYou selected {} folder(s):", to_delete.len());
    } else {
        let sel_size: u64 = to_delete.iter().filter_map(|i| i.size).sum();
        println!(
            "\nYou selected {} folder(s), ~{} will be freed:",
            to_delete.len(),
            human_size(sel_size)
        );
    }

    for item in &to_delete {
        println!(
            "  - {} ({})",
            rel_display(&item.path, &cfg.base_dir),
            format_size(item.size)
        );
    }

    if cfg.dry_run {
        println!("\nDry run complete. Nothing deleted.");
        return;
    }

    let confirm = match prompt_line("\nType 'yes' to permanently delete these: ") {
        Some(line) => line.trim().to_lowercase(),
        None => {
            println!("\nInterrupted. Nothing deleted.");
            return;
        }
    };

    if confirm != "yes" {
        println!("Aborted. Nothing deleted.");
        return;
    }

    let del_start = Instant::now();
    let next_idx = AtomicUsize::new(0);
    let freed = Mutex::new(0u64);
    let failed = Mutex::new(0usize);
    let output_lock = Mutex::new(());
    let num_workers = cfg.workers.min(to_delete.len().max(1));

    thread::scope(|s| {
        for _ in 0..num_workers {
            s.spawn(|| {
                loop {
                    let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                    if idx >= to_delete.len() {
                        break;
                    }
                    let item = to_delete[idx];
                    let rel = rel_display(&item.path, &cfg.base_dir);

                    if !is_safe_target(&item.path, &cfg.base_dir) {
                        let _g = output_lock.lock().unwrap();
                        println!("  ✘ SKIPPED {rel}: failed safety path check");
                        *failed.lock().unwrap() += 1;
                        continue;
                    }

                    match fs::remove_dir_all(&item.path) {
                        Ok(()) => {
                            if let Some(sz) = item.size {
                                *freed.lock().unwrap() += sz;
                            }
                            let _g = output_lock.lock().unwrap();
                            println!("  ✔ deleted {rel}");
                        }
                        Err(e) => {
                            *failed.lock().unwrap() += 1;
                            let _g = output_lock.lock().unwrap();
                            println!("  ✘ FAILED {rel}: {e}");
                        }
                    }
                }
            });
        }
    });

    let freed_total = freed.into_inner().unwrap();
    let failed_count = failed.into_inner().unwrap();
    if cfg.no_size {
        println!("\nDone in {:.2}s.", del_start.elapsed().as_secs_f64());
    } else {
        println!(
            "\nDone in {:.2}s. Freed ~{}.",
            del_start.elapsed().as_secs_f64(),
            human_size(freed_total)
        );
    }
    if failed_count > 0 {
        println!(
            "{failed_count} folder(s) could not be deleted (file in use? close editors/terminals/servers and retry)."
        );
    }
}

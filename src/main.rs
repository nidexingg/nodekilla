use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Instant, SystemTime};

struct Config {
    base_dir: PathBuf,
    sort_by_size: bool,
    dry_run: bool,
}

struct Item {
    project: String,
    path: PathBuf,
    size: u64,
    age_days: Option<u64>,
}

fn default_base_dir() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn parse_args() -> Result<Config, i32> {
    let mut base_dir = None;
    let mut sort_by_size = false;
    let mut dry_run = false;

    for arg in env::args().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                println!(
                    "Usage: nodekilla [TARGET_DIR] [--sort-size] [--dry-run]\n\n\
                     Options:\n  \
                       TARGET_DIR   Directory to scan (default: directory of executable)\n  \
                       --sort-size  Sort results by size descending\n  \
                       --dry-run    Simulate deletion without removing files\n  \
                       -h, --help   Show this help message"
                );
                return Err(0);
            }
            "--sort-size" | "-s" => sort_by_size = true,
            "--dry-run" | "-n" => dry_run = true,
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

    let base_dir = base_dir.unwrap_or_else(default_base_dir);
    let base_dir = fs::canonicalize(&base_dir).unwrap_or(base_dir);

    Ok(Config {
        base_dir,
        sort_by_size,
        dry_run,
    })
}

fn worker_count(task_count: usize) -> usize {
    let cores = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    cores.min(task_count.max(1))
}

/// Iterative walk to find all `node_modules` under `root` without following symlinks.
fn walk_for_node_modules(root: PathBuf) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root];

    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_symlink() || !ft.is_dir() {
                continue;
            }

            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str == "node_modules" {
                found.push(entry.path());
                continue;
            }
            if name_str.starts_with('.') {
                continue;
            }
            stack.push(entry.path());
        }
    }
    found
}

/// Iterative directory size calculation using `entry.metadata()` (no symlink following).
fn dir_size(root: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(entry.path());
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

fn age_in_days(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let elapsed = SystemTime::now().duration_since(modified).ok()?;
    Some(elapsed.as_secs() / 86_400)
}

fn scan_projects_parallel(base: &Path) -> Vec<Item> {
    let entries = match fs::read_dir(base) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut subdirs: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let ft = entry.file_type().ok()?;
            if ft.is_symlink() || !ft.is_dir() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            Some((name, entry.path()))
        })
        .collect();

    subdirs.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    if subdirs.is_empty() {
        return Vec::new();
    }

    let next_idx = AtomicUsize::new(0);
    let results = Mutex::new(Vec::<(usize, Item)>::new());
    let num_workers = worker_count(subdirs.len());

    thread::scope(|s| {
        for _ in 0..num_workers {
            s.spawn(|| {
                loop {
                    let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                    if idx >= subdirs.len() {
                        break;
                    }
                    let (proj, path) = &subdirs[idx];
                    let paths = walk_for_node_modules(path.clone());
                    let mut local = Vec::with_capacity(paths.len());
                    for nm_path in paths {
                        let size = dir_size(&nm_path);
                        let age_days = age_in_days(&nm_path);
                        local.push((
                            idx,
                            Item {
                                project: proj.clone(),
                                path: nm_path,
                                size,
                                age_days,
                            },
                        ));
                    }
                    if !local.is_empty() {
                        results.lock().unwrap().extend(local);
                    }
                }
            });
        }
    });

    let mut collected = results.into_inner().unwrap();
    collected.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.path.cmp(&b.1.path)));
    collected.into_iter().map(|(_, item)| item).collect()
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

fn format_age(days: Option<u64>) -> String {
    match days {
        Some(0) => "today".to_string(),
        Some(d) => format!("{d}d ago"),
        None => "?".to_string(),
    }
}

fn parse_selection(text: &str, max_num: usize) -> Vec<usize> {
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
                    for n in lo..=hi {
                        if n >= 1 && n <= max_num {
                            picked.insert(n);
                        }
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

    println!("\nScanning projects in: {}", cfg.base_dir.display());
    if cfg.dry_run {
        println!("(DRY RUN mode: no files will be deleted)");
    }

    let scan_start = Instant::now();
    let mut items = scan_projects_parallel(&cfg.base_dir);
    let scan_elapsed = scan_start.elapsed();

    if items.is_empty() {
        println!("\nNo node_modules folders found. Nothing to do. 🎉");
        return;
    }

    if cfg.sort_by_size {
        items.sort_by(|a, b| b.size.cmp(&a.size));
    }

    println!(
        "\nFound {} node_modules in {:.2}s:\n",
        items.len(),
        scan_elapsed.as_secs_f64()
    );
    for (idx, item) in items.iter().enumerate() {
        let i = idx + 1;
        let rel = rel_display(&item.path, &cfg.base_dir);
        println!(
            "  [{i:>2}] {:<20} {:>10}  {:>7}  ({rel})",
            item.project,
            human_size(item.size),
            format_age(item.age_days)
        );
    }

    let total: u64 = items.iter().map(|i| i.size).sum();
    println!("\n  Total reclaimable: {}", human_size(total));

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
    let sel_size: u64 = to_delete.iter().map(|i| i.size).sum();

    println!(
        "\nYou selected {} folder(s), ~{} will be freed:",
        to_delete.len(),
        human_size(sel_size)
    );
    for item in &to_delete {
        println!(
            "  - {} ({})",
            rel_display(&item.path, &cfg.base_dir),
            human_size(item.size)
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
    let num_workers = worker_count(to_delete.len());

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
                            *freed.lock().unwrap() += item.size;
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
    println!(
        "\nDone in {:.2}s. Freed ~{}.",
        del_start.elapsed().as_secs_f64(),
        human_size(freed_total)
    );
    if failed_count > 0 {
        println!(
            "{failed_count} folder(s) could not be deleted (file in use? close editors/terminals/servers and retry)."
        );
    }
}

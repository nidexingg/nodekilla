use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

fn base_dir() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn walk_for_node_modules(dir: &Path, found: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !file_type.is_dir() {
            continue;
        }

        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str == "node_modules" {
            found.push(entry.path());
            // Never descend into node_modules
            continue;
        }
        if name_str.starts_with('.') {
            // Never descend into hidden directories
            continue;
        }

        walk_for_node_modules(&entry.path(), found);
    }
}

/// Return Vec<(project_name, Vec<node_modules paths>)> for immediate subdirs of `base`.
fn find_node_modules(base: &Path) -> Vec<(String, Vec<PathBuf>)> {
    let entries = match fs::read_dir(base) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut subdirs: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let ft = entry.file_type().ok()?;
            if !ft.is_dir() {
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

    let mut results = Vec::new();
    for (name, path) in subdirs {
        let mut found = Vec::new();
        walk_for_node_modules(&path, &mut found);
        if !found.is_empty() {
            results.push((name, found));
        }
    }
    results
}

/// Total size of a directory in bytes (tolerant of locked/odd files).
fn dir_size(path: &Path) -> u64 {
    let entries = match fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    let mut total: u64 = 0;
    for entry in entries.flatten() {
        let entry_path = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if ft.is_dir() {
            total += dir_size(&entry_path);
        } else if let Ok(meta) = fs::metadata(&entry_path) {
            total += meta.len();
        }
    }
    total
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

/// Parse '1 3 5-7' style input into a sorted list of 1-based indexes.
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
                    for n in start..=end {
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

fn main() {
    let base = base_dir();
    println!("\nScanning projects in: {}\n", base.display());

    let projects = find_node_modules(&base);
    if projects.is_empty() {
        println!("No node_modules folders found. Nothing to do. 🎉");
        return;
    }

    // Build a flat, numbered list of every node_modules found
    let mut items: Vec<(String, PathBuf, u64)> = Vec::new();
    for (proj, paths) in projects {
        for p in paths {
            let size = dir_size(&p);
            items.push((proj.clone(), p, size));
        }
    }

    println!("Found node_modules:\n");
    for (idx, (proj, path, size)) in items.iter().enumerate() {
        let i = idx + 1;
        let rel = rel_display(path, &base);
        println!("  [{i}] {proj:<20} {:>10}  ({rel})", human_size(*size));
    }

    let total: u64 = items.iter().map(|(_, _, s)| *s).sum();
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

    let to_delete: Vec<&(String, PathBuf, u64)> = selected.iter().map(|&i| &items[i - 1]).collect();
    let sel_size: u64 = to_delete.iter().map(|(_, _, s)| *s).sum();

    println!(
        "\nYou selected {} folder(s), ~{} will be freed:",
        to_delete.len(),
        human_size(sel_size)
    );
    for (_, path, size) in &to_delete {
        println!("  - {} ({})", rel_display(path, &base), human_size(*size));
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

    let mut freed: u64 = 0;
    let mut failed: Vec<(&PathBuf, io::Error)> = Vec::new();

    for (_, path, size) in to_delete {
        match fs::remove_dir_all(path) {
            Ok(()) => {
                freed += *size;
                println!("  ✔ deleted {}", rel_display(path, &base));
            }
            Err(e) => {
                println!("  ✘ FAILED {}: {}", rel_display(path, &base), e);
                failed.push((path, e));
            }
        }
    }

    println!("\nDone. Freed ~{}.", human_size(freed));
    if !failed.is_empty() {
        println!(
            "{} folder(s) could not be deleted (file in use? close editors/terminals/servers and retry).",
            failed.len()
        );
    }
}
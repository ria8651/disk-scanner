//! Demo CLI for the scanner. The library is the product; this just proves it.

use disk_scanner::model::{Kind, State, ROOT};
use disk_scanner::report::{human, reclaimable_from_clones, reconcile, snapshot_hint};
use disk_scanner::sys::tcc::{self, Access};
use disk_scanner::{scan, Options};

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut root = PathBuf::from("/");
    let mut threads: Option<usize> = None;
    let mut top = 20usize;
    while let Some(a) = args.next() {
        match a.as_str() {
            "-j" | "--threads" => threads = args.next().and_then(|v| v.parse().ok()),
            "-n" | "--top" => top = args.next().and_then(|v| v.parse().ok()).unwrap_or(20),
            "-h" | "--help" => {
                eprintln!("usage: dscan [PATH] [-j THREADS] [-n TOP]");
                return;
            }
            other => root = PathBuf::from(other),
        }
    }

    // Full Disk Access is detectable but not requestable. Say so up front so a
    // partial scan is never mistaken for a complete one.
    match tcc::full_disk_access() {
        Access::Full => eprintln!("Full Disk Access: granted"),
        Access::Denied => {
            eprintln!("Full Disk Access: DENIED — results will be incomplete.");
            if let Some(app) = tcc::responsible_app_hint() {
                eprintln!("  Grant it to: {app}");
            }
            eprintln!("  System Settings > Privacy & Security > Full Disk Access");
            eprintln!("  {}", tcc::SETTINGS_URL);
        }
        Access::Unknown => eprintln!("Full Disk Access: could not determine"),
    }

    let mut opts = Options::default();
    if let Some(t) = threads {
        opts.threads = t;
    }
    eprintln!("scanning {} with {} threads…", root.display(), opts.threads);

    let cancel = Arc::new(AtomicBool::new(false));
    {
        // Ctrl-C cancels cleanly and still prints what we have.
        let _ = CANCEL.set(Arc::clone(&cancel));
        unsafe {
            libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
        }
    }

    // Only animate when stderr is a terminal; otherwise a redirected run
    // accumulates thousands of progress lines.
    let interactive = unsafe { libc::isatty(libc::STDERR_FILENO) == 1 };
    let progress: Option<Box<dyn Fn(disk_scanner::Progress) + Send>> = if interactive {
        Some(Box::new(|p: disk_scanner::Progress| {
            eprint!(
                "\r  {} dirs  {} files  {}    ",
                p.dirs,
                p.files,
                human(p.physical)
            );
            let _ = std::io::stderr().flush();
        }))
    } else {
        None
    };

    let r = match scan(&root, opts, cancel, progress) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("\nscan failed: {e}");
            std::process::exit(1);
        }
    };
    if interactive {
        eprintln!("\r{:60}\r", "");
    }

    let t = &r.tree;
    let root_node = t.node(ROOT);
    let s = &r.stats;

    println!("=== {} ===", r.root_path.display());
    if r.cancelled {
        println!("!! cancelled — partial results");
    }
    println!(
        "{} dirs, {} files, {} symlinks in {:.2}s ({} nodes, ~{} arena)",
        s.dirs,
        s.files,
        s.symlinks,
        s.elapsed.as_secs_f64(),
        t.len(),
        human(t.memory_bytes() as u64)
    );

    println!("\n-- size --");
    println!(
        "  apparent (logical)   {:>12}",
        human(root_node.size.logical)
    );
    println!(
        "  allocated (physical) {:>12}",
        human(root_node.size.physical)
    );
    println!(
        "  exclusively owned    {:>12}",
        human(root_node.size.exclusive)
    );
    println!(
        "     shared w/ clones  {:>12}",
        human(root_node.size.shared_clones)
    );
    println!(
        "     pinned by snapshot{:>12}",
        human(root_node.size.shared_snapshot)
    );
    println!(
        "     sparse holes      {:>12}",
        human(root_node.size.sparse_saving)
    );

    println!("\n-- composition --");
    println!(
        "  {} hardlink aliases folded, {} clone members, {} dataless (iCloud), \
         {} compressed, {} sparse",
        s.hardlink_aliases, s.clone_members, s.dataless, s.compressed, s.sparse
    );

    let (clone_bytes, whole, total_fams) = reclaimable_from_clones(&r);
    if total_fams > 0 {
        println!(
            "  {} clone families ({} wholly inside this scan → {} extra reclaimable)",
            total_fams,
            whole,
            human(clone_bytes)
        );
    }

    if s.denied_tcc + s.denied_perm + s.errors > 0 || root_node.unknown_below > 0 {
        println!("\n-- incomplete --");
        println!(
            "  {} TCC-denied, {} permission-denied, {} errors; \
             {} nodes below root are unaccounted",
            s.denied_tcc, s.denied_perm, s.errors, root_node.unknown_below
        );
        println!("  totals above are a LOWER BOUND.");
    }

    if !r.skipped.is_empty() {
        println!("\n-- mounts not entered --");
        for (p, why) in &r.skipped {
            println!("  {:<52} {}", p.display(), why);
        }
    }

    if let Some(rec) = reconcile(&r) {
        println!("\n-- reconciliation --");
        println!("  walked (allocated)   {:>12}", human(rec.walked_physical));
        println!("  volume used (statfs) {:>12}", human(rec.volume_used));
        for line in wrap(&rec.explain(), 74) {
            println!("  {line}");
        }
    }

    if let Some(m) = snapshot_hint(&r).message() {
        println!("\n-- why 'exclusively owned' is so low --");
        for line in wrap(&m, 76) {
            println!("  {line}");
        }
    }

    println!("\n-- largest directories (allocated, subtree) --");
    let mut dirs: Vec<u32> = (0..t.len() as u32)
        .filter(|&i| t.node(i).is_dir())
        .collect();
    dirs.sort_unstable_by_key(|&i| std::cmp::Reverse(t.node(i).size.physical));
    for &id in dirs.iter().take(top) {
        let n = t.node(id);
        let mark = match n.state() {
            State::Ok if n.unknown_below == 0 => ' ',
            State::Ok => '~',
            _ => '!',
        };
        println!(
            "{} {:>10}  excl {:>10}  {}",
            mark,
            human(n.size.physical),
            human(n.size.exclusive),
            t.path(id).display()
        );
    }

    println!("\n-- largest files (allocated) --");
    let mut files: Vec<u32> = (0..t.len() as u32)
        .filter(|&i| matches!(t.node(i).kind(), Kind::File))
        .collect();
    files.sort_unstable_by_key(|&i| std::cmp::Reverse(t.node(i).size.physical));
    for &id in files.iter().take(top) {
        let n = t.node(id);
        println!(
            "  {:>10}  excl {:>10}  {}",
            human(n.size.physical),
            human(n.size.exclusive),
            t.path(id).display()
        );
    }
}

fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > w {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

static CANCEL: std::sync::OnceLock<Arc<AtomicBool>> = std::sync::OnceLock::new();

extern "C" fn on_sigint(_: libc::c_int) {
    if let Some(c) = CANCEL.get() {
        c.store(true, Ordering::Relaxed);
    }
}

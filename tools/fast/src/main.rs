use std::{
    io::{IsTerminal, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use base64::Engine as _;
use clap::{Parser, Subcommand};
use fast::{TestCase, engine::run_case};
use file_server::FileServer;
use rand::prelude::*;
use simply_colored::{BLUE, GREEN, RED, RESET, YELLOW};

#[derive(Subcommand)]
enum Command {
    /// Run all test cases.
    RunAll,
    /// Run specific test cases (matched as substrings of their names).
    Run {
        /// Space delimited list of test case names.
        #[arg(value_delimiter = ' ', num_args = 1..)]
        tests: Vec<String>,
    },
    /// Run test cases in a random order forever, until interrupted or one fails.
    Stress,
}

#[derive(Parser)]
struct Cli {
    /// The host address of the receiver.
    #[arg(long, short('H'), default_value_t = String::from("127.0.0.1"))]
    host: String,
    /// The port of the receiver.
    #[arg(long, short, default_value_t = 46899)]
    port: u16,
    #[arg(long, short, default_value_t = String::from("../fcast-sample-media"))]
    sample_media_dir: String,
    /// The receiver's certificate fingerprint for v4.
    /// When omitted, any server certificate is accepted during the v4 TLS
    /// upgrade.
    #[arg(long, short)]
    fingerprint: Option<String>,
    /// Case-name substring to exclude (repeatable; applies to `run-all` and
    /// `stress`). Soak around a known-flaky case without it aborting the run.
    #[arg(long, global = true)]
    exclude: Vec<String>,
    /// Case-name substring to include (repeatable; applies to `run-all` and
    /// `stress`). When given, only matching cases run; `--exclude` still
    /// applies on top.
    #[arg(long, global = true)]
    only: Vec<String>,
    #[command(flatten)]
    verbosity: clap_verbosity_flag::Verbosity<clap_verbosity_flag::OffLevel>,
    #[command(subcommand)]
    command: Command,
}

/// Erase the current line so a fresh one can replace an in-place status /
/// partial line. A no-op byte-wise when stdout is not a terminal, where
/// nothing is rewritten in place.
fn clear_line() -> &'static str {
    if std::io::stdout().is_terminal() {
        "\x1B[2K\r"
    } else {
        ""
    }
}

/// `HH:MM:SS` for status lines (`Duration`'s Debug prints raw seconds).
fn fmt_elapsed(elapsed: Duration) -> String {
    let s = elapsed.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Run a single test case, printing its result. Returns `true` on success.
async fn run_test(
    case: &TestCase,
    receiver: &SocketAddr,
    file_server: &FileServer,
    sample_media: &Path,
    fingerprint: Option<&[u8]>,
) -> bool {
    file_server.clear();

    match run_case(
        receiver,
        file_server,
        sample_media,
        case.steps,
        fingerprint.map(<[u8]>::to_vec),
    )
    .await
    {
        Ok(()) => true,
        Err(err) => {
            println!("{}test {} ... {RED}FAILED{RESET}", clear_line(), case.name);
            println!("Reason: {err:?}");
            println!("==================== DUMPING STATE ====================");
            file_server.dump_to_stdout();
            println!("=======================================================");
            false
        }
    }
}

async fn run_tests(
    receiver: SocketAddr,
    sample_media: PathBuf,
    targets: Vec<String>,
    fingerprint: Option<Vec<u8>>,
) {
    let file_server = FileServer::new(0).await.unwrap();
    let mut stdout = std::io::stdout();

    for target in &targets {
        let matched: Vec<_> = fast::TEST_CASES
            .iter()
            .filter(|t| t.name.contains(target.as_str()))
            .collect();

        if matched.is_empty() {
            println!("test {target} ... {YELLOW}SKIPPED{RESET}");
            println!("Reason: no test case matches \"{target}\"");
            continue;
        }

        for case in matched {
            // The partial line only makes sense where it can be rewritten.
            if std::io::stdout().is_terminal() {
                print!("test {} ...", case.name);
                stdout.flush().unwrap();
            }

            if !run_test(
                case,
                &receiver,
                &file_server,
                &sample_media,
                fingerprint.as_deref(),
            )
            .await
            {
                return;
            }

            println!("{}test {} ... {GREEN}OK{RESET}", clear_line(), case.name);
        }
    }
}

async fn stress(
    receiver: SocketAddr,
    sample_media: PathBuf,
    fingerprint: Option<Vec<u8>>,
    exclude: Vec<String>,
    only: Vec<String>,
) {
    let should_run = Arc::new(AtomicBool::new(true));
    ctrlc::set_handler({
        let should_run = Arc::clone(&should_run);
        move || should_run.store(false, Ordering::Relaxed)
    })
    .unwrap();

    let file_server = FileServer::new(0).await.unwrap();
    let mut stdout = std::io::stdout();
    let mut passed = 0u64;
    let mut rng = rand::rng();
    let mut indices: Vec<usize> = (0..fast::TEST_CASES.len())
        .filter(|&i| {
            let name = fast::TEST_CASES[i].name;
            (only.is_empty() || only.iter().any(|o| name.contains(o.as_str())))
                && !exclude.iter().any(|e| name.contains(e.as_str()))
        })
        .collect();
    if indices.is_empty() {
        eprintln!("stress: no test case matches the --only/--exclude filters");
        std::process::exit(2);
    }
    if !exclude.is_empty() || !only.is_empty() {
        println!(
            "stress: filters selected {} of {} case(s)",
            indices.len(),
            fast::TEST_CASES.len()
        );
    }
    let is_tty = std::io::stdout().is_terminal();
    let start = Instant::now();
    let mut last_log_status = start;
    let mut failed = false;

    'out: loop {
        indices.shuffle(&mut rng);
        for &idx in &indices {
            if !should_run.load(Ordering::Relaxed) {
                break 'out;
            }
            let name = fast::TEST_CASES[idx].name;

            // The status carries the case ABOUT to run, so both a human and
            // an external stall-watcher can tell which case is in flight and
            // since when. On a terminal it is ONE line, rewritten in place;
            // piped to a log it is one plain greppable line per case.
            if is_tty {
                print!(
                    "\x1B[2K\r[ {BLUE}{}{RESET} | {GREEN}{passed}{RESET} passed | {:.1}/s ] running {name}",
                    fmt_elapsed(start.elapsed()),
                    passed as f64 / start.elapsed().as_secs_f64().max(0.001),
                );
            } else {
                println!("stress: starting {name}");
            }
            stdout.flush().unwrap();

            if !run_test(
                &fast::TEST_CASES[idx],
                &receiver,
                &file_server,
                &sample_media,
                fingerprint.as_deref(),
            )
            .await
            {
                failed = true;
                break 'out;
            }

            passed += 1;

            // Piped runs get a periodic summary line too (in-place status
            // lines would just bloat the log).
            if !is_tty && last_log_status.elapsed() >= Duration::from_secs(10) {
                println!(
                    "stress: {passed} passed, elapsed {}",
                    fmt_elapsed(start.elapsed())
                );
                last_log_status = Instant::now();
            }
        }
    }

    if is_tty && !failed {
        // Leave the in-place status line behind.
        println!();
    }
    println!(
        "stress: {passed} case(s) passed in {} ({:.1}/s){}",
        fmt_elapsed(start.elapsed()),
        passed as f64 / start.elapsed().as_secs_f64().max(0.001),
        if failed { ", then FAILED" } else { "" },
    );
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let sample_media = PathBuf::from(cli.sample_media_dir);

    tracing_subscriber::fmt()
        .with_max_level(cli.verbosity)
        .init();

    let receiver = SocketAddr::new(cli.host.parse().expect("invalid host address"), cli.port);

    let fingerprint = cli.fingerprint.map(|fp| {
        base64::engine::general_purpose::STANDARD
            .decode(fp.trim())
            .expect("invalid base64 fingerprint")
    });

    match cli.command {
        Command::RunAll => {
            let all = fast::TEST_CASES
                .iter()
                .map(|t| t.name)
                .filter(|name| {
                    cli.only.is_empty() || cli.only.iter().any(|o| name.contains(o.as_str()))
                })
                .filter(|name| !cli.exclude.iter().any(|e| name.contains(e.as_str())))
                .map(str::to_string)
                .collect();
            run_tests(receiver, sample_media, all, fingerprint).await;
        }
        Command::Run { tests } => run_tests(receiver, sample_media, tests, fingerprint).await,
        Command::Stress => stress(receiver, sample_media, fingerprint, cli.exclude, cli.only).await,
    }
}

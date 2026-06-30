use std::{
    io::Write,
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
    #[command(flatten)]
    verbosity: clap_verbosity_flag::Verbosity<clap_verbosity_flag::OffLevel>,
    #[command(subcommand)]
    command: Command,
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
            println!("\rtest {} ... {RED}FAILED{RESET}", case.name);
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
            print!("test {} ...", case.name);
            stdout.flush().unwrap();

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

            println!("\rtest {} ... {GREEN}OK{RESET}", case.name);
        }
    }
}

async fn stress(receiver: SocketAddr, sample_media: PathBuf, fingerprint: Option<Vec<u8>>) {
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
    let mut indices: Vec<usize> = (0..fast::TEST_CASES.len()).collect();
    let start = Instant::now();
    let mut last_status_dump = start;

    'out: loop {
        indices.shuffle(&mut rng);
        for &idx in &indices {
            if !should_run.load(Ordering::Relaxed) {
                break 'out;
            }

            if !run_test(
                &fast::TEST_CASES[idx],
                &receiver,
                &file_server,
                &sample_media,
                fingerprint.as_deref(),
            )
            .await
            {
                break 'out;
            }

            passed += 1;

            if last_status_dump.elapsed() >= Duration::from_secs(1) {
                print!(
                    "\x1B[2K\r[ Elapsed: {BLUE}{:?}{RESET} Tests ran: {GREEN}{passed}{RESET} ]",
                    start.elapsed()
                );
                stdout.flush().unwrap();
                last_status_dump = Instant::now();
            }
        }
    }

    println!("\n{passed} test cases ran and passed");
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
                .map(|t| t.name.to_string())
                .collect();
            run_tests(receiver, sample_media, all, fingerprint).await;
        }
        Command::Run { tests } => run_tests(receiver, sample_media, tests, fingerprint).await,
        Command::Stress => stress(receiver, sample_media, fingerprint).await,
    }
}

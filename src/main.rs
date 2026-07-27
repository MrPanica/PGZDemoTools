use clap::{Parser, Subcommand, ValueEnum};
use main_error::MainError;
use pgz_demo_tools::demo::{DemoKind, normalize_ranges, parse_time_range, read_demo, safe_name};
use pgz_demo_tools::edit::{edit_demo, split_demo};
use pgz_demo_tools::voice::{create_zip, export_voices};
use pgz_demo_tools::web;
use std::env;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "PGZDemoTools",
    about = "TF2 POV/SourceTV demo editor and voice exporter"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Clone, Copy, ValueEnum)]
enum AudioFormat {
    Ogg,
    Wav,
    Mp3,
}

impl AudioFormat {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ogg => "ogg",
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Start the local web editor.
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 8765)]
        port: u16,
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long)]
        no_browser: bool,
    },
    /// Show demo metadata.
    Info { demo: PathBuf },
    /// Extract one time range.
    Cut {
        demo: PathBuf,
        #[arg(long = "from")]
        start: f64,
        #[arg(long = "to")]
        end: f64,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Join ordered time ranges from one POV/SourceTV demo.
    Montage {
        demo: PathBuf,
        #[arg(long = "range", required = true)]
        ranges: Vec<String>,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Compatibility alias for the production SourceTV montage.
    #[command(name = "source-montage-test")]
    SourceMontageTest {
        demo: PathBuf,
        #[arg(long = "range", required = true)]
        ranges: Vec<String>,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Split into independent parts.
    Split {
        demo: PathBuf,
        #[arg(long, conflicts_with = "seconds")]
        parts: Option<u32>,
        #[arg(long, conflicts_with = "parts")]
        seconds: Option<f64>,
        #[arg(short = 'o', long = "output-dir")]
        output_dir: Option<PathBuf>,
    },
    /// Export player voices.
    Voice {
        demo: PathBuf,
        #[arg(long = "player")]
        players: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        no_gaps: bool,
        #[arg(long, value_enum, default_value_t = AudioFormat::Ogg)]
        format: AudioFormat,
        #[arg(long)]
        archive: bool,
        #[arg(short = 'o', long = "output-dir")]
        output_dir: Option<PathBuf>,
    },
    /// Compatibility command; helpers are built into this executable.
    BuildHelper,
    /// Run the built-in compatibility check.
    SelfTest,
}

fn default_workspace() -> PathBuf {
    if let Some(path) = env::var_os("PGZ_DEMO_WORKSPACE") {
        return PathBuf::from(path);
    }
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".work")
}

fn tick(value: f64, tick_rate: f64) -> Result<u32, MainError> {
    if !value.is_finite() || value < 0.0 {
        return Err("invalid edit range".into());
    }
    let value = (value * tick_rate).round_ties_even();
    if value > u32::MAX as f64 {
        return Err("edit range is too large".into());
    }
    Ok(value as u32)
}

fn ranges(values: &[String], tick_rate: f64) -> Result<Vec<(u32, u32)>, MainError> {
    values
        .iter()
        .map(|value| {
            let (start, end) = parse_time_range(value)?;
            Ok((tick(start, tick_rate)?, tick(end, tick_rate)?))
        })
        .collect()
}

fn self_test() -> Result<(), MainError> {
    if normalize_ranges(&[(5, 9), (1, 3), (3, 6)], 10, false)? != [(1, 9)]
        || normalize_ranges(&[(5, 9), (1, 3)], 10, true)? != [(5, 9), (1, 3)]
        || safe_name(" тест?.dem ", "output") != "тест_.dem"
    {
        return Err("self-check failed".into());
    }
    println!("self-check: OK");
    Ok(())
}

fn run(command: Option<Command>) -> Result<(), MainError> {
    match command {
        None => web::serve("127.0.0.1", 8765, &default_workspace(), false),
        Some(Command::Serve {
            host,
            port,
            workspace,
            no_browser,
        }) => web::serve(
            &host,
            port,
            &workspace.unwrap_or_else(default_workspace),
            no_browser,
        ),
        Some(Command::SelfTest) => self_test(),
        Some(Command::Info { demo }) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&read_demo(demo)?.meta())?
            );
            Ok(())
        }
        Some(Command::Cut {
            demo,
            start,
            end,
            output,
        }) => {
            let info = read_demo(&demo)?;
            let target = output.unwrap_or_else(|| {
                demo.with_file_name(format!(
                    "{}.cut.dem",
                    demo.file_stem().unwrap_or_default().to_string_lossy()
                ))
            });
            edit_demo(
                &info,
                &[(tick(start, info.tick_rate)?, tick(end, info.tick_rate)?)],
                &target,
                &default_workspace(),
            )?;
            println!("{}", target.display());
            Ok(())
        }
        Some(Command::Montage {
            demo,
            ranges: values,
            output,
        }) => {
            let info = read_demo(&demo)?;
            let target = output.unwrap_or_else(|| {
                demo.with_file_name(format!(
                    "{}.montage.dem",
                    demo.file_stem().unwrap_or_default().to_string_lossy()
                ))
            });
            let ranges = ranges(&values, info.tick_rate)?;
            edit_demo(&info, &ranges, &target, &default_workspace())?;
            println!("{}", target.display());
            Ok(())
        }
        Some(Command::SourceMontageTest {
            demo,
            ranges: values,
            output,
        }) => {
            let info = read_demo(&demo)?;
            if info.kind != DemoKind::SourceTv {
                return Err("source-montage-test accepts SourceTV demos only".into());
            }
            let target = output.unwrap_or_else(|| {
                demo.with_file_name(format!(
                    "{}.source-test.dem",
                    demo.file_stem().unwrap_or_default().to_string_lossy()
                ))
            });
            let ranges = ranges(&values, info.tick_rate)?;
            edit_demo(&info, &ranges, &target, &default_workspace())?;
            println!("{}", target.display());
            Ok(())
        }
        Some(Command::Split {
            demo,
            parts,
            seconds,
            output_dir,
        }) => {
            let output = output_dir.unwrap_or_else(|| {
                demo.with_file_name(format!(
                    "{}_parts",
                    demo.file_stem().unwrap_or_default().to_string_lossy()
                ))
            });
            let targets = split_demo(
                &demo,
                &output,
                Some(parts.unwrap_or(5)),
                seconds,
                &default_workspace(),
            )?;
            println!("Created {} parts in {}", targets.len(), output.display());
            Ok(())
        }
        Some(Command::Voice {
            demo,
            players,
            all,
            no_gaps,
            format,
            archive,
            output_dir,
        }) => {
            if !all && players.is_empty() {
                return Err("use --player NAME/ID or --all".into());
            }
            let info = read_demo(&demo)?;
            let output = output_dir.unwrap_or_else(|| {
                demo.with_file_name(format!(
                    "{}_voices",
                    demo.file_stem().unwrap_or_default().to_string_lossy()
                ))
            });
            let targets = export_voices(
                &info,
                &output,
                &players,
                all,
                !no_gaps,
                format.as_str(),
                &default_workspace(),
            )?;
            if archive {
                let archive = output.join("voices.zip");
                create_zip(&targets, &archive)?;
                println!("{}", archive.display());
            }
            println!(
                "Created {} voice tracks in {}",
                targets.len(),
                output.display()
            );
            Ok(())
        }
        Some(Command::BuildHelper) => {
            println!(
                "{}",
                env::current_exe()
                    .unwrap_or_else(|_| PathBuf::from("PGZDemoTools"))
                    .display()
            );
            println!("POV and SourceTV helpers are built in");
            Ok(())
        }
    }
}

fn main() -> Result<(), MainError> {
    run(Cli::parse().command)
}

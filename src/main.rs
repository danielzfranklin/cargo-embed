mod config;
mod error;
mod helpers;
mod logging;
mod rttui;

use structopt;

use anyhow::{anyhow, Context, Result};
use chrono::Local;
use colored::*;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::{
    env,
    fs::File,
    io::Write,
    iter, panic,
    path::{Path, PathBuf},
    process::{self, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use structopt::StructOpt;

use probe_rs::{
    config::TargetSelector,
    flashing::{download_file_with_options, DownloadOptions, FlashProgress, Format, ProgressEvent},
    DebugProbeSelector, Probe,
};
use probe_rs_rtt::{Rtt, ScanRegion};

#[derive(Debug, StructOpt)]
struct Opt {
    #[structopt(name = "config")]
    config: Option<String>,
    #[structopt(name = "list-chips", long = "list-chips")]
    list_chips: bool,
    #[structopt(name = "disable-progressbars", long = "disable-progressbars")]
    disable_progressbars: bool,

    // `cargo build` arguments
    #[structopt(name = "binary", long = "bin")]
    bin: Option<String>,
    #[structopt(name = "example", long = "example")]
    example: Option<String>,
    #[structopt(name = "package", short = "p", long = "package")]
    package: Option<String>,
    #[structopt(name = "release", long = "release")]
    release: bool,
    #[structopt(name = "target", long = "target")]
    target: Option<String>,
    #[structopt(name = "PATH", long = "manifest-path", parse(from_os_str))]
    manifest_path: Option<PathBuf>,
    #[structopt(long)]
    no_default_features: bool,
    #[structopt(long)]
    all_features: bool,
    #[structopt(long)]
    features: Vec<String>,
}

const ARGUMENTS_TO_REMOVE: &[&str] = &["list-chips", "disable-progressbars"];

fn main() {
    match main_try() {
        Ok(_) => (),
        Err(e) => {
            // Ensure stderr is flushed before calling proces::exit,
            // otherwise the process might panic, because it tries
            // to access stderr during shutdown.
            //
            // We ignore the errors, not much we can do anyway.

            let mut stderr = std::io::stderr();

            let first_line_prefix = "Error".red().bold();
            let other_line_prefix: String = iter::repeat(" ")
                .take(first_line_prefix.chars().count())
                .collect();

            let error = format!("{:?}", e);

            for (i, line) in error.lines().enumerate() {
                let _ = write!(stderr, "       ");

                if i == 0 {
                    let _ = write!(stderr, "{}", first_line_prefix);
                } else {
                    let _ = write!(stderr, "{}", other_line_prefix);
                };

                let _ = writeln!(stderr, " {}", line);
            }

            let _ = stderr.flush();

            process::exit(1);
        }
    }
}

fn main_try() -> Result<()> {
    let mut args = std::env::args();

    // When called by Cargo, the first argument after the binary name will be `embed`. If that's the
    // case, remove one argument (`Opt::from_iter` will remove the binary name by itself).
    if env::args().nth(1) == Some("embed".to_string()) {
        args.next();
    }

    let mut args: Vec<_> = args.collect();

    // Get commandline options.
    let opt = Opt::from_iter(&args);

    // Get the config.
    let config_name = opt.config.as_deref().unwrap_or_else(|| "default");
    let config = config::Configs::new(config_name)
        .map_err(|e| anyhow!("The config could not be loaded: {}", e))?;

    logging::init(Some(config.general.log_level));

    // Make sure we load the config given in the cli parameters.
    for cdp in &config.general.chip_descriptions {
        probe_rs::config::registry::add_target_from_yaml(&Path::new(cdp))
            .with_context(|| format!("failed to load the chip description from {}", cdp))?;
    }

    let chip = if opt.list_chips {
        print_families()?;
        std::process::exit(0);
    } else {
        config
            .general
            .chip
            .as_ref()
            .map(|chip| chip.into())
            .unwrap_or(TargetSelector::Auto)
    };

    let name = args[3].clone();

    // Remove executable name from the arguments list.
    args.remove(0);

    // Remove all arguments that `cargo build` does not understand.
    helpers::remove_arguments(ARGUMENTS_TO_REMOVE, &mut args);

    if let Some(index) = args.iter().position(|x| x == config_name) {
        // We remove the argument we found.
        args.remove(index);
    }

    let status = Command::new("cargo")
        .arg("build")
        .args(args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?
        .wait()?;

    if !status.success() {
        handle_failed_command(status)
    }

    // Try and get the cargo project information.
    let project = cargo_project::Project::query(".")
        .map_err(|e| anyhow!("failed to parse Cargo project information: {}", e))?;

    // Decide what artifact to use.
    let artifact = if let Some(bin) = &opt.bin {
        cargo_project::Artifact::Bin(bin)
    } else if let Some(example) = &opt.example {
        cargo_project::Artifact::Example(example)
    } else {
        cargo_project::Artifact::Bin(project.name())
    };

    // Decide what profile to use.
    let profile = if opt.release {
        cargo_project::Profile::Release
    } else {
        cargo_project::Profile::Dev
    };

    // Try and get the artifact path.
    let path = project
        .path(
            artifact,
            profile,
            opt.target.as_ref().map(|t| &**t),
            "x86_64-unknown-linux-gnu",
        )
        .map_err(|e| anyhow!("Couldn't get artifact path: {}", e))?;

    logging::println(format!(
        "    {} {}",
        "Flashing".green().bold(),
        path.display()
    ));

    // If we got a probe selector in the config, open the probe matching the selector if possible.
    let mut probe = match (config.probe.usb_vid.as_ref(), config.probe.usb_pid.as_ref()) {
        (Some(vid), Some(pid)) => {
            let selector = DebugProbeSelector {
                vendor_id: u16::from_str_radix(vid, 16)?,
                product_id: u16::from_str_radix(pid, 16)?,
                serial_number: config.probe.serial.clone(),
            };
            Probe::open(selector)?
        }
        _ => {
            if let Some(_) = config.probe.usb_vid {
                log::warn!("USB VID ignored, because PID is not specified.");
            }
            if let Some(_) = config.probe.usb_pid {
                log::warn!("USB PID ignored, because VID is not specified.");
            }

            // Only automatically select a probe if there is only
            // a single probe detected.
            let list = Probe::list_all();
            if list.len() > 1 {
                return Err(anyhow!("More than a single probe detected. Use the --probe-selector argument to select which probe to use."));
            }

            Probe::open(
                list.first()
                    .ok_or_else(|| anyhow!("No supported probe was found"))?,
            )?
        }
    };

    probe
        .select_protocol(config.probe.protocol)
        .context("failed to select protocol")?;

    let protocol_speed = if let Some(speed) = config.probe.speed {
        let actual_speed = probe.set_speed(speed).context("failed to set speed")?;

        if actual_speed < speed {
            log::warn!(
                "Unable to use specified speed of {} kHz, actual speed used is {} kHz",
                speed,
                actual_speed
            );
        }

        actual_speed
    } else {
        probe.speed_khz()
    };

    log::info!("Protocol speed {} kHz", protocol_speed);

    let mut session = probe.attach(chip).context("failed attaching to target")?;

    if config.flashing.enabled {
        // Start timer.
        let instant = Instant::now();

        if !opt.disable_progressbars {
            // Create progress bars.
            let multi_progress = MultiProgress::new();
            let style = ProgressStyle::default_bar()
                .tick_chars("⠁⠁⠉⠙⠚⠒⠂⠂⠒⠲⠴⠤⠄⠄⠤⠠⠠⠤⠦⠖⠒⠐⠐⠒⠓⠋⠉⠈⠈✔")
                .progress_chars("##-")
                .template("{msg:.green.bold} {spinner} [{elapsed_precise}] [{wide_bar}] {bytes:>8}/{total_bytes:>8} @ {bytes_per_sec:>10} (eta {eta:3})");

            // Create a new progress bar for the fill progress if filling is enabled.
            let fill_progress = if config.flashing.restore_unwritten_bytes {
                let fill_progress = Arc::new(multi_progress.add(ProgressBar::new(0)));
                fill_progress.set_style(style.clone());
                fill_progress.set_message("     Reading flash  ");
                Some(fill_progress)
            } else {
                None
            };

            // Create a new progress bar for the erase progress.
            let erase_progress = Arc::new(multi_progress.add(ProgressBar::new(0)));
            {
                logging::set_progress_bar(erase_progress.clone());
            }
            erase_progress.set_style(style.clone());
            erase_progress.set_message("     Erasing sectors");

            // Create a new progress bar for the program progress.
            let program_progress = multi_progress.add(ProgressBar::new(0));
            program_progress.set_style(style);
            program_progress.set_message(" Programming pages  ");

            let flash_layout_output_path = config.flashing.flash_layout_output_path.clone();
            // Register callback to update the progress.
            let progress = FlashProgress::new(move |event| {
                use ProgressEvent::*;
                match event {
                    Initialized { flash_layout } => {
                        let total_page_size: u32 =
                            flash_layout.pages().iter().map(|s| s.size()).sum();
                        let total_sector_size: u32 =
                            flash_layout.sectors().iter().map(|s| s.size()).sum();
                        let total_fill_size: u32 =
                            flash_layout.fills().iter().map(|s| s.size()).sum();
                        if let Some(fp) = fill_progress.as_ref() {
                            fp.set_length(total_fill_size as u64)
                        }
                        erase_progress.set_length(total_sector_size as u64);
                        program_progress.set_length(total_page_size as u64);
                        let visualizer = flash_layout.visualize();
                        flash_layout_output_path
                            .as_ref()
                            .map(|path| visualizer.write_svg(path));
                    }
                    StartedFlashing => {
                        program_progress.enable_steady_tick(100);
                        program_progress.reset_elapsed();
                    }
                    StartedErasing => {
                        erase_progress.enable_steady_tick(100);
                        erase_progress.reset_elapsed();
                    }
                    StartedFilling => {
                        if let Some(fp) = fill_progress.as_ref() {
                            fp.enable_steady_tick(100)
                        };
                        if let Some(fp) = fill_progress.as_ref() {
                            fp.reset_elapsed()
                        };
                    }
                    PageFlashed { size, .. } => {
                        program_progress.inc(size as u64);
                    }
                    SectorErased { size, .. } => {
                        erase_progress.inc(size as u64);
                    }
                    PageFilled { size, .. } => {
                        if let Some(fp) = fill_progress.as_ref() {
                            fp.inc(size as u64)
                        };
                    }
                    FailedErasing => {
                        erase_progress.abandon();
                        program_progress.abandon();
                    }
                    FinishedErasing => {
                        erase_progress.finish();
                    }
                    FailedProgramming => {
                        program_progress.abandon();
                    }
                    FinishedProgramming => {
                        program_progress.finish();
                    }
                    FailedFilling => {
                        if let Some(fp) = fill_progress.as_ref() {
                            fp.abandon()
                        };
                    }
                    FinishedFilling => {
                        if let Some(fp) = fill_progress.as_ref() {
                            fp.finish()
                        };
                    }
                }
            });

            // Make the multi progresses print.
            // indicatif requires this in a separate thread as this join is a blocking op,
            // but is required for printing multiprogress.
            let progress_thread_handle = std::thread::spawn(move || {
                multi_progress.join().unwrap();
            });

            download_file_with_options(
                &mut session,
                path.as_path(),
                Format::Elf,
                DownloadOptions {
                    progress: Some(&progress),
                    keep_unwritten_bytes: config.flashing.restore_unwritten_bytes,
                },
            )
            .with_context(|| format!("failed to flash {}", path.display()))?;

            // We don't care if we cannot join this thread.
            let _ = progress_thread_handle.join();

            // If we don't do this, the inactive progress bars will swallow log
            // messages, so they'll never be printed anywhere.
            logging::clear_progress_bar();
        } else {
            download_file_with_options(
                &mut session,
                path.as_path(),
                Format::Elf,
                DownloadOptions {
                    progress: None,
                    keep_unwritten_bytes: config.flashing.restore_unwritten_bytes,
                },
            )
            .with_context(|| format!("failed to flash {}", path.display()))?;
        }

        // Stop timer.
        let elapsed = instant.elapsed();
        logging::println(format!(
            "    {} in {}s",
            "Finished".green().bold(),
            elapsed.as_millis() as f32 / 1000.0,
        ));

        let mut core = session.core(0)?;
        if config.flashing.halt_afterwards {
            core.reset_and_halt(Duration::from_millis(500))?;
        } else {
            core.reset()?;
        }
    }

    if config.gdb.enabled && config.rtt.enabled {
        return Err(anyhow!(
            "Unfortunately, at the moment, only GDB OR RTT are possible."
        ));
    }

    if config.gdb.enabled {
        let gdb_connection_string = config
            .gdb
            .gdb_connection_string
            .as_deref()
            .or_else(|| Some("localhost:1337"));
        // This next unwrap will always resolve as the connection string is always Some(T).
        logging::println(format!(
            "Firing up GDB stub at {}.",
            gdb_connection_string.as_ref().unwrap(),
        ));
        if let Err(e) = probe_rs_gdb_server::run(gdb_connection_string, session) {
            logging::eprintln("During the execution of GDB an error was encountered:");
            logging::eprintln(format!("{:?}", e));
        }
    } else if config.rtt.enabled {
        let session = Arc::new(Mutex::new(session));
        let t = std::time::Instant::now();
        let mut error = None;

        let mut i = 1;

        while (t.elapsed().as_millis() as usize) < config.rtt.timeout {
            log::info!("Initializing RTT (attempt {})...", i);
            i += 1;

            let rtt_header_address = if let Ok(mut file) = File::open(path.as_path()) {
                if let Some(address) = rttui::app::App::get_rtt_symbol(&mut file) {
                    ScanRegion::Exact(address as u32)
                } else {
                    ScanRegion::Ram
                }
            } else {
                ScanRegion::Ram
            };

            match Rtt::attach_region(session.clone(), &rtt_header_address) {
                Ok(rtt) => {
                    log::info!("RTT initialized.");

                    // `App` puts the terminal into a special state, as required
                    // by the text-based UI. If a panic happens while the
                    // terminal is in that state, this will completely mess up
                    // the user's terminal (misformatted panic message, newlines
                    // being ignored, input characters not being echoed, ...).
                    //
                    // The following panic hook cleans up the terminal, while
                    // otherwise preserving the behavior of the default panic
                    // hook (or whichever custom hook might have been registered
                    // before).
                    let previous_panic_hook = panic::take_hook();
                    panic::set_hook(Box::new(move |panic_info| {
                        rttui::app::clean_up_terminal();
                        previous_panic_hook(panic_info);
                    }));

                    let logname = format!("{}_{}", name, Local::now().to_rfc3339());

                    let mut app = rttui::app::App::new(rtt, &config, logname)?;
                    loop {
                        app.poll_rtt();
                        app.render();
                        if app.handle_event() {
                            logging::println("Shutting down.");
                            return Ok(());
                        };
                    }
                }
                Err(err) => {
                    error = Some(anyhow!("Error attaching to RTT: {}", err));
                }
            };

            log::debug!("Failed to initialize RTT. Retrying until timeout.");
        }
        if let Some(error) = error {
            return Err(error);
        }
    }

    Ok(())
}

fn print_families() -> Result<()> {
    logging::println("Available chips:");
    for family in probe_rs::config::registry::families()
        .map_err(|e| anyhow!("Families could not be read: {:?}", e))?
    {
        logging::println(&family.name);
        logging::println("    Variants:");
        for variant in family.variants() {
            logging::println(format!("        {}", variant.name));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn handle_failed_command(status: std::process::ExitStatus) -> ! {
    use std::os::unix::process::ExitStatusExt;
    let status = status.code().or_else(|| status.signal()).unwrap_or(1);
    std::process::exit(status)
}

#[cfg(not(unix))]
fn handle_failed_command(status: std::process::ExitStatus) -> ! {
    let status = status.code().unwrap_or(1);
    std::process::exit(status)
}

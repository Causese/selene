use std::{
    fmt, fs,
    io::{self, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};

use codespan_reporting::{diagnostic::Severity as CodespanSeverity, term::DisplayStyle};
use full_moon::ast::owned::Owned;
use selene_lib::{rules::Severity, standard_library::StandardLibrary, *};
use structopt::StructOpt;
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};
use threadpool::ThreadPool;

mod opts;

macro_rules! error {
    ($fmt:expr) => {
        error(fmt::format(format_args!($fmt))).unwrap();
    };

    ($fmt:expr, $($args:tt)*) => {
        error(fmt::format(format_args!($fmt, $($args)*))).unwrap();
    };
}

static QUIET: AtomicBool = AtomicBool::new(false);

static LINT_ERRORS: AtomicUsize = AtomicUsize::new(0);
static LINT_WARNINGS: AtomicUsize = AtomicUsize::new(0);
static PARSE_ERRORS: AtomicUsize = AtomicUsize::new(0);

fn error(text: String) -> io::Result<()> {
    let mut stderr = StandardStream::stderr(ColorChoice::Auto);
    stderr.set_color(ColorSpec::new().set_fg(Some(Color::Red)))?;
    write!(&mut stderr, "ERROR: ")?;
    stderr.set_color(ColorSpec::new().set_fg(None))?;
    writeln!(&mut stderr, "{}", text)?;
    Ok(())
}

fn log_total(parse_errors: usize, lint_errors: usize, lint_warnings: usize) -> io::Result<()> {
    let mut stdout = StandardStream::stdout(ColorChoice::Auto);

    stdout.set_color(ColorSpec::new().set_fg(None))?;
    writeln!(&mut stdout, "Results:")?;

    let mut stat = |number: usize, label: &str| -> io::Result<()> {
        if number > 0 {
            stdout.set_color(ColorSpec::new().set_fg(Some(Color::Red)))?;
        } else {
            stdout.set_color(ColorSpec::new().set_fg(None))?;
        }

        write!(&mut stdout, "{}", number)?;
        stdout.set_color(ColorSpec::new().set_fg(None))?;
        writeln!(&mut stdout, " {}", label)
    };

    stat(lint_errors, "errors")?;
    stat(lint_warnings, "warnings")?;
    stat(parse_errors, "parse errors")?;

    Ok(())
}

fn read_file(checker: &Checker<toml::value::Value>, filename: &Path) {
    let contents = match fs::read_to_string(filename) {
        Ok(contents) => contents,
        Err(error) => {
            error!(
                "Couldn't read contents of file {}: {}",
                filename.display(),
                error
            );
            return;
        }
    };

    let ast = match full_moon::parse(&contents) {
        Ok(ast) => ast.owned(),
        Err(error) => {
            // TODO: Use codespan for this
            PARSE_ERRORS.fetch_add(1, Ordering::Release);
            error!("Error parsing {}: {}", filename.display(), error);
            return;
        }
    };

    let mut diagnostics = checker.test_on(&ast);
    diagnostics.sort_by_key(|diagnostic| diagnostic.diagnostic.start_position());

    let mut files = codespan::Files::new();
    let source_id = files.add(filename.to_string_lossy(), contents);

    let stdout = termcolor::StandardStream::stdout(termcolor::ColorChoice::Auto);
    let mut stdout = stdout.lock();

    let (mut errors, mut warnings) = (0, 0);
    for diagnostic in &diagnostics {
        match diagnostic.severity {
            Severity::Error => errors += 1,
            Severity::Warning => warnings += 1,
        };
    }

    LINT_ERRORS.fetch_add(errors, Ordering::Release);
    LINT_WARNINGS.fetch_add(warnings, Ordering::Release);

    for diagnostic in diagnostics.into_iter().map(|diagnostic| {
        diagnostic.diagnostic.into_codespan_diagnostic(
            source_id,
            match diagnostic.severity {
                Severity::Error => CodespanSeverity::Error,
                Severity::Warning => CodespanSeverity::Warning,
            },
        )
    }) {
        codespan_reporting::term::emit(
            &mut stdout,
            &codespan_reporting::term::Config {
                display_style: if QUIET.load(Ordering::Relaxed) {
                    DisplayStyle::Short
                } else {
                    DisplayStyle::Rich
                },
                ..Default::default()
            },
            &files,
            &diagnostic,
        )
        .expect("couldn't emit to codespan");
    }
}

fn main() {
    let matches = opts::Options::from_args();

    QUIET.store(matches.quiet, Ordering::Relaxed);

    let config: CheckerConfig<toml::value::Value> = match matches.config {
        Some(config_file) => {
            let config_contents = match fs::read_to_string(config_file) {
                Ok(contents) => contents,
                Err(error) => {
                    error!("Couldn't read config file: {}", error);
                    return;
                }
            };

            match toml::from_str(&config_contents) {
                Ok(config) => config,
                Err(error) => {
                    error!("Config file not in correct format: {}", error);
                    return;
                }
            }
        }

        None => match fs::read_to_string("selene.toml") {
            Ok(config_contents) => match toml::from_str(&config_contents) {
                Ok(config) => config,
                Err(error) => {
                    error!("Config file not in correct format: {}", error);
                    return;
                }
            },

            Err(_) => CheckerConfig::default(),
        },
    };

    let standard_library = match fs::read_to_string(format!("{}.toml", &config.std)) {
        Ok(contents) => match toml::from_str::<StandardLibrary>(&contents) {
            Ok(mut standard_library) => {
                standard_library.inflate();
                standard_library
            }
            Err(error) => {
                error!(
                    "Custom standard library wasn't formatted properly: {}",
                    error
                );
                return;
            }
        },

        Err(_) => match StandardLibrary::from_name(&config.std) {
            Some(std) => std,
            None => {
                error!("Unknown standard library '{}'", config.std);
                return;
            }
        },
    };

    let checker = Arc::new(match Checker::new(config, standard_library) {
        Ok(checker) => checker,
        Err(error) => {
            error!("{}", error);
            return;
        }
    });

    let pool = ThreadPool::new(matches.num_threads);

    for filename in &matches.files {
        match fs::metadata(filename) {
            Ok(metadata) => {
                if metadata.is_file() {
                    let checker = Arc::clone(&checker);
                    let filename = filename.to_owned();

                    pool.execute(move || read_file(&checker, Path::new(&filename)));
                } else if metadata.is_dir() {
                    let glob =
                        match glob::glob(&format!("{}/{}", filename.to_string_lossy(), matches.pattern)) {
                            Ok(glob) => glob,
                            Err(error) => {
                                error!("Invalid glob pattern: {}", error);
                                return;
                            }
                        };

                    for entry in glob {
                        match entry {
                            Ok(path) => {
                                let checker = Arc::clone(&checker);

                                pool.execute(move || read_file(&checker, &path));
                            }

                            Err(error) => {
                                error!(
                                    "Couldn't open file {}: {}",
                                    filename.to_string_lossy(),
                                    error
                                );
                            }
                        };
                    }
                } else {
                    unreachable!("Somehow got a symlink from the files?");
                }
            }

            Err(error) => {
                error!(
                    "Error getting metadata of {}: {}",
                    filename.to_string_lossy(),
                    error
                );
            }
        };
    }

    pool.join();

    let (parse_errors, lint_errors, lint_warnings) = (
        PARSE_ERRORS.load(Ordering::Relaxed),
        LINT_ERRORS.load(Ordering::Relaxed),
        LINT_WARNINGS.load(Ordering::Relaxed),
    );

    log_total(parse_errors, lint_errors, lint_warnings).ok();

    if parse_errors + lint_errors + lint_warnings > 0 {
        std::process::exit(1);
    }
}
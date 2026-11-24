mod body;
mod crawler;
mod mime;
mod server;
mod store;
mod systemd;

use std::env::{self, VarError};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

const TOKEN_ENV: &str = "STATIC_FILE_HOST_TOKEN";

const HELP: &str = "\
static-file-host: upload files with a token, view them publicly by random id

USAGE:
    static-file-host [serve] [OPTIONS]    run the server (default)
    static-file-host token [--data DIR]   print the upload token
    static-file-host systemd [OPTIONS]    print a systemd unit for this directory and user

OPTIONS:
    --listen ADDR       address to listen on           [default: 127.0.0.1:8080]
    --data DIR          where files and the token live [default: data]
    --public-url URL    base of returned urls, e.g. https://files.example.com
                        [default: from X-Forwarded-Proto/X-Forwarded-Host/Host]
    --threads N         event loop threads             [default: 1]
    --token TOKEN       upload token; other users can see it in ps
                        [default: $STATIC_FILE_HOST_TOKEN, else DIR/token]

UPLOAD:
    curl -H 'Authorization: Bearer TOKEN' -F file=@photo.png http://HOST/
    curl -H 'Authorization: Bearer TOKEN' -T Dockerfile http://HOST/

INSTALL AS SERVICE:
    static-file-host systemd | sudo tee /etc/systemd/system/static-file-host.service
    sudo systemctl enable --now static-file-host
";

enum Command {
    Serve,
    Token,
    Systemd,
    Help,
}

pub struct Options {
    pub listen: SocketAddr,
    pub data: PathBuf,
    pub public_url: Option<String>,
    pub threads: usize,
    pub token: Option<String>,
}

fn main() -> ExitCode {
    let result = parse_args().and_then(|(command, options)| match command {
        Command::Serve => serve(options),
        Command::Token => upload_token(&options).map(|(token, _)| println!("{token}")),
        Command::Systemd => systemd::unit(&options).map(|unit| print!("{unit}")),
        Command::Help => {
            print!("{HELP}");
            Ok(())
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn serve(options: Options) -> Result<(), String> {
    let store = store::Store::open(options.data.clone())
        .map_err(|e| format!("opening {}: {e}", options.data.display()))?;
    let (token, created) = upload_token(&options)?;
    if created {
        eprintln!("created upload token: {token}");
    }

    let mut runtime = if options.threads == 1 {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(options.threads);
        builder
    };
    let runtime = runtime
        .enable_io()
        .enable_time()
        .max_blocking_threads(64)
        .build()
        .map_err(|e| format!("starting runtime: {e}"))?;

    let app = server::App::new(store, token, options.public_url);
    runtime
        .block_on(server::run(Box::leak(Box::new(app)), options.listen))
        .map_err(|e| format!("listening on {}: {e}", options.listen))
}

/// Returns the token and whether it was just created in the data directory.
fn upload_token(options: &Options) -> Result<(String, bool), String> {
    let (token, source) = match (&options.token, env::var(TOKEN_ENV)) {
        (Some(token), _) => (token.clone(), "--token"),
        (None, Ok(token)) => (token, TOKEN_ENV),
        (None, Err(VarError::NotUnicode(_))) => {
            return Err(format!("{TOKEN_ENV} is not valid UTF-8"));
        }
        (None, Err(VarError::NotPresent)) => {
            return store::load_or_create_token(&options.data)
                .map_err(|e| format!("reading token: {e}"));
        }
    };
    let token = token.trim();
    if token.is_empty() {
        return Err(format!("{source} is empty"));
    }
    Ok((token.to_owned(), false))
}

fn parse_args() -> Result<(Command, Options), String> {
    let mut args = env::args().skip(1).peekable();
    let command = match args.peek().map(String::as_str) {
        Some("serve") => Command::Serve,
        Some("token") => Command::Token,
        Some("systemd") => Command::Systemd,
        Some("help") => Command::Help,
        _ => return parse_options(args).map(|options| (Command::Serve, options)),
    };
    args.next();
    parse_options(args).map(|options| (command, options))
}

fn parse_options(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
        data: PathBuf::from("data"),
        public_url: None,
        threads: 1,
        token: None,
    };
    while let Some(arg) = args.next() {
        if arg == "-h" || arg == "--help" {
            print!("{HELP}");
            std::process::exit(0);
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
            None => (arg, None),
        };
        if !matches!(
            flag.as_str(),
            "--listen" | "--data" | "--public-url" | "--threads" | "--token"
        ) {
            return Err(format!("unknown option {flag}, see --help"));
        }
        let value = match inline {
            Some(value) => value,
            None => args.next().ok_or_else(|| format!("{flag} needs a value"))?,
        };
        match flag.as_str() {
            "--listen" => {
                options.listen = value
                    .parse()
                    .map_err(|e| format!("--listen {value}: {e}"))?
            }
            "--data" => options.data = PathBuf::from(value),
            "--public-url" => options.public_url = Some(value.trim_end_matches('/').to_owned()),
            "--threads" => {
                options.threads = value
                    .parse()
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or_else(|| format!("--threads {value}: expected a positive number"))?
            }
            "--token" => options.token = Some(value),
            _ => unreachable!(),
        }
    }
    Ok(options)
}

use std::env;

use crate::Options;

pub fn unit(options: &Options) -> Result<String, String> {
    let dir = env::current_dir().map_err(|e| format!("reading current directory: {e}"))?;
    let exe = env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .map_err(|e| format!("locating executable: {e}"))?;
    let user = env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .map_err(|_| "cannot tell the current user: USER is not set".to_owned())?;

    let mut args = vec![
        exe.display().to_string(),
        "serve".into(),
        "--listen".into(),
        options.listen.to_string(),
        "--data".into(),
        options.data.display().to_string(),
        "--threads".into(),
        options.threads.to_string(),
    ];
    if let Some(url) = &options.public_url {
        args.extend(["--public-url".into(), url.clone()]);
    }
    let exec_start: Vec<String> = args
        .iter()
        .map(|arg| quote(&arg.replace('$', "$$")))
        .collect();

    let capabilities = if options.listen.port() < 1024 {
        "CAP_NET_BIND_SERVICE"
    } else {
        ""
    };

    Ok(format!(
        "\
[Unit]
Description=Static file host
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User={user}
WorkingDirectory={working_directory}
ExecStart={exec_start}
Restart=always
RestartSec=2
Environment=MALLOC_ARENA_MAX=2
LimitNOFILE=65536
UMask=0077
AmbientCapabilities={capabilities}

[Install]
WantedBy=multi-user.target
",
        working_directory = dir.display().to_string().replace('%', "%%"),
        exec_start = exec_start.join(" "),
    ))
}

// systemd expands %specifiers in every setting and splits unquoted words on whitespace.
fn quote(word: &str) -> String {
    let word = word.replace('%', "%%");
    if word
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | ';'))
    {
        format!("\"{}\"", word.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        word
    }
}

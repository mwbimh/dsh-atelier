use std::{
    env,
    io::{self, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    process::ExitCode,
    str::FromStr,
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const VERSION: &str = "0.0.0-fake";
const LOOPBACK_HOST: &str = "127.0.0.1";
const DEFAULT_DELAY_MS: u64 = 250;
const DEFAULT_CRASH_DELAY_MS: u64 = 100;
const MODE_ENV: &str = "FAKE_DSH_MODE";
const DELAY_ENV: &str = "FAKE_DSH_DELAY_MS";
const CRASH_DELAY_ENV: &str = "FAKE_DSH_CRASH_DELAY_MS";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Mode {
    #[default]
    Normal,
    DelayedReady,
    InvalidUrl,
    Crash,
    ExitBeforeReady,
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "normal" => Ok(Self::Normal),
            "delayed-ready" => Ok(Self::DelayedReady),
            "invalid-url" => Ok(Self::InvalidUrl),
            "crash" => Ok(Self::Crash),
            "exit-before-ready" => Ok(Self::ExitBeforeReady),
            other => Err(format!(
                "unsupported fake DSH mode {other:?}; expected normal, delayed-ready, invalid-url, crash, or exit-before-ready"
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Command {
    Version,
    Web(WebOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WebOptions {
    host: IpAddr,
    port: u16,
    mode: Mode,
    delay: Duration,
    crash_delay: Duration,
}

impl WebOptions {
    fn from_args_and_env(
        args: impl IntoIterator<Item = String>,
        env_value: impl Fn(&str) -> Option<String>,
    ) -> Result<Command, String> {
        let mut args = args.into_iter();
        let Some(first) = args.next() else {
            return Err("expected --version or web".into());
        };

        if first == "--version" || first == "-V" {
            if args.next().is_some() {
                return Err("--version does not accept arguments".into());
            }
            return Ok(Command::Version);
        }
        if first != "web" {
            return Err(format!("unsupported command {first:?}; expected web"));
        }

        let mut host = LOOPBACK_HOST
            .parse::<IpAddr>()
            .expect("the fixed loopback address is valid");
        let mut port = 3080;
        let mut mode = env_value(MODE_ENV)
            .map(|value| value.parse())
            .transpose()?
            .unwrap_or_default();
        let mut delay = duration_from_env(&env_value, DELAY_ENV, DEFAULT_DELAY_MS)?;
        let mut crash_delay =
            duration_from_env(&env_value, CRASH_DELAY_ENV, DEFAULT_CRASH_DELAY_MS)?;

        while let Some(flag) = args.next() {
            let mut value = || {
                args.next()
                    .ok_or_else(|| format!("{flag} requires a value"))
            };
            match flag.as_str() {
                "--host" => {
                    let raw = value()?;
                    host = raw
                        .parse()
                        .map_err(|_| format!("invalid --host value {raw:?}"))?;
                }
                "--port" => {
                    let raw = value()?;
                    port = raw
                        .parse()
                        .map_err(|_| format!("invalid --port value {raw:?}"))?;
                }
                "--mode" | "--fake-mode" => mode = value()?.parse()?,
                "--delay-ms" => delay = parse_duration(&flag, &value()?)?,
                "--crash-delay-ms" => crash_delay = parse_duration(&flag, &value()?)?,
                other => return Err(format!("unsupported argument {other:?}")),
            }
        }

        if host != IpAddr::V4(Ipv4Addr::LOCALHOST) {
            return Err(format!(
                "--host must be {LOOPBACK_HOST}; refusing to bind to {host}"
            ));
        }

        Ok(Command::Web(Self {
            host,
            port,
            mode,
            delay,
            crash_delay,
        }))
    }
}

fn duration_from_env(
    env_value: &impl Fn(&str) -> Option<String>,
    name: &str,
    default_ms: u64,
) -> Result<Duration, String> {
    match env_value(name) {
        Some(raw) => parse_duration(name, &raw),
        None => Ok(Duration::from_millis(default_ms)),
    }
}

fn parse_duration(name: &str, raw: &str) -> Result<Duration, String> {
    raw.parse::<u64>()
        .map(Duration::from_millis)
        .map_err(|_| format!("invalid {name} value {raw:?}; expected milliseconds"))
}

#[tokio::main]
async fn main() -> ExitCode {
    let command =
        match WebOptions::from_args_and_env(env::args().skip(1), |name| env::var(name).ok()) {
            Ok(command) => command,
            Err(error) => {
                eprintln!("fake-dsh: {error}");
                return ExitCode::from(2);
            }
        };

    match command {
        Command::Version => {
            println!("{VERSION}");
            ExitCode::SUCCESS
        }
        Command::Web(options) => match run_web(options).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("fake-dsh: {error}");
                ExitCode::from(1)
            }
        },
    }
}

async fn run_web(options: WebOptions) -> io::Result<()> {
    if options.mode == Mode::ExitBeforeReady {
        return Err(io::Error::other("exiting before readiness as requested"));
    }

    let listener = TcpListener::bind(SocketAddr::new(options.host, options.port)).await?;
    let address = listener.local_addr()?;

    if options.mode == Mode::DelayedReady {
        tokio::time::sleep(options.delay).await;
    }

    let advertised_url = match options.mode {
        Mode::InvalidUrl => format!("http://example.invalid:{}", address.port()),
        _ => format!("http://{LOOPBACK_HOST}:{}", address.port()),
    };
    print_readiness(&advertised_url)?;

    if options.mode == Mode::Crash {
        tokio::time::sleep(options.crash_delay).await;
        return Err(io::Error::other("crashing after readiness as requested"));
    }

    serve(listener).await
}

fn print_readiness(url: &str) -> io::Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    writeln!(stdout, "dsh web: {url}")?;
    stdout.flush()
}

async fn serve(listener: TcpListener) -> io::Result<()> {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                tokio::spawn(async move {
                    let _ = respond(stream).await;
                });
            }
            result = tokio::signal::ctrl_c() => {
                result?;
                return Ok(());
            }
        }
    }
}

async fn respond(mut stream: TcpStream) -> io::Result<()> {
    let mut request = [0_u8; 8192];
    let _ = stream.read(&mut request).await?;
    let body = b"<!doctype html><title>Fake DSH</title><h1>Fake DSH is ready</h1>";
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn parse(args: &[&str], environment: &[(&str, &str)]) -> Result<Command, String> {
        let environment = environment
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>();
        WebOptions::from_args_and_env(args.iter().map(|value| (*value).to_owned()), |name| {
            environment.get(name).cloned()
        })
    }

    #[test]
    fn accepts_the_dsh_web_command() {
        assert_eq!(
            parse(&["web", "--host", "127.0.0.1", "--port", "0"], &[]),
            Ok(Command::Web(WebOptions {
                host: "127.0.0.1".parse().unwrap(),
                port: 0,
                mode: Mode::Normal,
                delay: Duration::from_millis(DEFAULT_DELAY_MS),
                crash_delay: Duration::from_millis(DEFAULT_CRASH_DELAY_MS),
            }))
        );
    }

    #[test]
    fn supports_each_failure_mode_from_the_environment() {
        for (raw, expected) in [
            ("delayed-ready", Mode::DelayedReady),
            ("invalid-url", Mode::InvalidUrl),
            ("crash", Mode::Crash),
            ("exit-before-ready", Mode::ExitBeforeReady),
        ] {
            let Command::Web(options) = parse(
                &["web", "--host", "127.0.0.1", "--port", "0"],
                &[(MODE_ENV, raw)],
            )
            .unwrap() else {
                panic!("expected web command");
            };
            assert_eq!(options.mode, expected);
        }
    }

    #[test]
    fn command_line_mode_overrides_the_environment() {
        let Command::Web(options) =
            parse(&["web", "--mode", "crash"], &[(MODE_ENV, "invalid-url")]).unwrap()
        else {
            panic!("expected web command");
        };
        assert_eq!(options.mode, Mode::Crash);
    }

    #[test]
    fn refuses_non_loopback_bindings() {
        assert_eq!(
            parse(&["web", "--host", "0.0.0.0", "--port", "0"], &[]),
            Err("--host must be 127.0.0.1; refusing to bind to 0.0.0.0".into())
        );
    }

    #[tokio::test]
    async fn serves_an_http_success_response() {
        let listener = TcpListener::bind((LOOPBACK_HOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            respond(stream).await.unwrap();
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        server.await.unwrap();

        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with("<h1>Fake DSH is ready</h1>"));
    }
}

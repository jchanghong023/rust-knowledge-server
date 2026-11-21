use std::{
    ffi::OsString,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    num::NonZeroUsize,
    path::PathBuf,
};

use reqwest::{Url, header::HeaderValue};

use crate::{model_http::ApiConfig, search::FusionMode};

const DEFAULT_LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9333);

pub(crate) const HELP: &str = "\
Rust Knowledge Server：从 Web 导入 Markdown，通过 Web 和只读 MCP 搜索。

用法：
  rust-knowledge-server [选项]
  rust-knowledge-server --edge-probe <write|read> <目录>

启动选项（不读取服务配置文件）：
  -h, --help                 显示本帮助并退出
  --listen <IP:PORT>         HTTP 监听；默认 0.0.0.0:9333（所有 IPv4 接口），IPv6 可用 [::]:9333
  --mcp-origin <ORIGIN>      允许的 MCP HTTP(S) 来源，不带路径，可重复；默认不允许携带 Origin
                             不发送 Origin 的客户端仍可连接，但必须匹配 Host
  --fusion <grouped|edge>    混合检索方式；默认 grouped（先分组再融合）
                             edge 使用原生 RRF 后分组；切换不需要重导

固定数据目录：
  Windows x64：~/.rust-knowledge-server（本地磁盘，机器重启后保留）
  CentOS 7 x64：/dev/shm/rust-knowledge-server（tmpfs，无需 root）
               进程重启可复用，机器重启后清空；请自行保管 Markdown 并重新导入。
  同一目录只能由一个进程使用。

模型选项（均默认关闭，支持 HTTP(S) API，互不依赖）：
  --embedding-url <URL>         Embedding API 基址，可带 /v1
  --embedding-model <NAME>      Embedding 模型名
  --embedding-dimensions <N>    正整数，须与模型维度相同；启用时前三项必填
  --embedding-api-key <KEY>     可选 Bearer 密钥
  --rerank-url <URL>             Rerank API 基址，可带 /v1
  --rerank-model <NAME>          Rerank 模型名；启用时 URL 与模型名均必填
  --rerank-api-key <KEY>        可选 Bearer 密钥
  启用或更换 Embedding 后，从原始 Markdown 重导以生成兼容向量。
  密钥作为命令行参数可能被进程列表或 shell 历史记录看到。

如何使用：
  本机：直接启动，打开 http://127.0.0.1:9333；登记 8 位数字工号后建库、导入
        UTF-8 .md 文件，再在「测试搜索」查询。重导会整库替换。
  远程访问：默认可用 http://<服务IP>:9333。服务不验证身份，请用防火墙限制可访问的机器。
        仅本机使用时，指定 --listen 127.0.0.1:9333。
  Claude Code：claude mcp add --transport http knowledge http://127.0.0.1:9333/mcp
  OMP：运行 /mcp add，选择 HTTP，填同一个 /mcp 地址；已预置地址的构建可直接用。
  MCP 只读且无需登记工号；连接共享实例时使用管理员提供的 /mcp 地址。
  --edge-probe：write 写入测试索引，read 验证重开；使用单独的测试目录。
";

#[cfg(target_os = "linux")]
pub(crate) const DAEMON_HELP: &str = "

CentOS 7 后台管理（无需 root）：
  rust-knowledge-server start [启动选项]  后台启动，监听就绪后返回
  rust-knowledge-server status           查看后台实例的 PID 和监听地址
  rust-knowledge-server stop             正常停机并等待收尾
  直接运行仍为前台模式；后台日志：/dev/shm/rust-knowledge-server/daemon.log。
  不负责异常退出后拉起或开机自启。
";

#[cfg(target_os = "linux")]
pub(crate) const DAEMON_CHILD: &str = "__daemon";

pub(crate) enum Command {
    Help,
    Probe {
        mode: &'static str,
        directory: PathBuf,
    },
    Serve(Settings),
    #[cfg(target_os = "linux")]
    Start,
    #[cfg(target_os = "linux")]
    Status,
    #[cfg(target_os = "linux")]
    Stop,
    #[cfg(target_os = "linux")]
    Daemon(Settings),
}

pub(crate) struct Settings {
    pub(crate) listen: SocketAddr,
    pub(crate) mcp_origins: Vec<String>,
    pub(crate) embedding: Option<EmbeddingSettings>,
    pub(crate) rerank: Option<ApiConfig>,
    pub(crate) fusion: FusionMode,
}

pub(crate) struct EmbeddingSettings {
    pub(crate) api: ApiConfig,
    pub(crate) dimensions: usize,
}

#[derive(Default)]
struct ModelArgs {
    url: Option<String>,
    name: Option<String>,
    key: Option<String>,
}

impl ModelArgs {
    fn finish(self, flags: &str, required: bool) -> io::Result<Option<ApiConfig>> {
        match (self.url, self.name, self.key) {
            (None, None, None) if !required => Ok(None),
            (Some(base_url), Some(model), api_key) => Ok(Some(ApiConfig {
                base_url,
                model,
                api_key,
            })),
            _ => Err(invalid(flags, "URL and model name are required together")),
        }
    }
}

pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> io::Result<Command> {
    let mut args = args.into_iter().peekable();
    if args.peek().is_some_and(|arg| arg == "--edge-probe") {
        args.next();
        if args
            .peek()
            .is_some_and(|arg| arg == "--help" || arg == "-h")
        {
            return Ok(Command::Help);
        }
        let mode = match value(&mut args, "--edge-probe")?.to_str() {
            Some("write") => "write",
            Some("read") => "read",
            _ => return Err(invalid("--edge-probe", "expected write or read")),
        };
        let directory = value(&mut args, "--edge-probe")?.into();
        if args.next().is_some() {
            return Err(invalid("--edge-probe", "unexpected argument"));
        }
        return Ok(Command::Probe { mode, directory });
    }
    #[cfg(target_os = "linux")]
    let daemon_command = match args.peek().and_then(|arg| arg.to_str()) {
        Some("start") => Some(false),
        Some(DAEMON_CHILD) => Some(true),
        Some("status" | "stop") => {
            let command = if args.next().is_some_and(|arg| arg == "status") {
                Command::Status
            } else {
                Command::Stop
            };
            return match args.next() {
                None => Ok(command),
                Some(arg) if (arg == "--help" || arg == "-h") && args.next().is_none() => {
                    Ok(Command::Help)
                }
                _ => Err(invalid(
                    "arguments",
                    "status and stop take no startup options",
                )),
            };
        }
        _ => None,
    };
    #[cfg(target_os = "linux")]
    if daemon_command.is_some() {
        args.next();
    }

    let (mut listen, mut fusion, mut dimensions) = (None, None, None);
    let mut mcp_origins = Vec::new();
    let (mut embedding, mut rerank) = (ModelArgs::default(), ModelArgs::default());
    while let Some(option) = args.next() {
        let unknown = || invalid("arguments", "unknown option or unexpected argument");
        let flag = option.to_str().ok_or_else(unknown)?;
        let mut text = || {
            value(&mut args, flag)?
                .into_string()
                .map_err(|_| invalid(flag, "value must be valid Unicode"))
        };
        match flag {
            "-h" | "--help" => return Ok(Command::Help),
            "--listen" => set_once(
                &mut listen,
                text()?
                    .parse()
                    .map_err(|_| invalid(flag, "requires an IP address and port"))?,
                flag,
            )?,
            "--mcp-origin" => mcp_origins.push(canonical_origin(&text()?, flag)?),
            "--fusion" => set_once(
                &mut fusion,
                match text()?.as_str() {
                    "grouped" => FusionMode::Grouped,
                    "edge" => FusionMode::Edge,
                    _ => return Err(invalid(flag, "expected grouped or edge")),
                },
                flag,
            )?,
            "--embedding-url" => set_once(&mut embedding.url, text()?, flag)?,
            "--embedding-model" => set_once(&mut embedding.name, text()?, flag)?,
            "--embedding-api-key" => set_once(&mut embedding.key, text()?, flag)?,
            "--embedding-dimensions" => set_once(
                &mut dimensions,
                text()?
                    .parse::<NonZeroUsize>()
                    .map_err(|_| invalid(flag, "requires a positive integer"))?
                    .get(),
                flag,
            )?,
            "--rerank-url" => set_once(&mut rerank.url, text()?, flag)?,
            "--rerank-model" => set_once(&mut rerank.name, text()?, flag)?,
            "--rerank-api-key" => set_once(&mut rerank.key, text()?, flag)?,
            _ => return Err(unknown()),
        }
    }
    let listen = listen.unwrap_or(DEFAULT_LISTEN);
    let embedding = embedding
        .finish("--embedding-url / --embedding-model", dimensions.is_some())?
        .map(|api| {
            let dimensions = dimensions.ok_or_else(|| {
                invalid(
                    "--embedding-dimensions",
                    "required when Embedding is enabled",
                )
            })?;
            Ok::<_, io::Error>(EmbeddingSettings { api, dimensions })
        })
        .transpose()?;
    let settings = Settings {
        listen,
        mcp_origins,
        embedding,
        rerank: rerank.finish("--rerank-url / --rerank-model", false)?,
        fusion: fusion.unwrap_or_default(),
    };
    #[cfg(target_os = "linux")]
    if let Some(child) = daemon_command {
        return Ok(if child {
            Command::Daemon(settings)
        } else {
            Command::Start
        });
    }
    Ok(Command::Serve(settings))
}

fn value(args: &mut impl Iterator<Item = OsString>, flag: &str) -> io::Result<OsString> {
    args.next()
        .filter(|value| !value.is_empty() && !value.as_encoded_bytes().starts_with(b"--"))
        .ok_or_else(|| invalid(flag, "missing value"))
}

fn set_once<T>(slot: &mut Option<T>, value: T, flag: &str) -> io::Result<()> {
    if slot.is_some() {
        return Err(invalid(flag, "may only be specified once"));
    }
    *slot = Some(value);
    Ok(())
}

fn invalid(field: &str, reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("{field}: {reason}"))
}

fn canonical_origin(value: &str, field: &str) -> io::Result<String> {
    let Some((scheme, authority)) = value.split_once("://") else {
        return Err(invalid(field, "must be an explicit HTTP or HTTPS origin"));
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(invalid(field, "scheme must be HTTP or HTTPS"));
    }
    if authority.contains('@') {
        return Err(invalid(field, "credentials are not allowed"));
    }
    if authority.contains(['/', '\\', '?', '#']) {
        return Err(invalid(field, "path, query, and fragment are not allowed"));
    }
    if HttpAuthority::parse(authority).is_none() {
        return Err(invalid(field, "invalid host or port"));
    }
    let origin = Url::parse(value)
        .map_err(|_| invalid(field, "invalid HTTP(S) origin"))?
        .origin()
        .ascii_serialization();
    HeaderValue::from_str(&origin).map_err(|_| invalid(field, "not a valid HTTP header value"))?;
    Ok(origin)
}

enum Host<'a> {
    Ip(IpAddr),
    Dns(&'a str),
}

pub(crate) struct HttpAuthority<'a> {
    host: Host<'a>,
    port: Option<u16>,
}

impl<'a> HttpAuthority<'a> {
    pub(crate) fn parse(authority: &'a str) -> Option<Self> {
        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            let (host, tail) = bracketed.split_once(']')?;
            let port = if tail.is_empty() {
                None
            } else {
                Some(parse_port(tail.strip_prefix(':')?)?)
            };
            (Host::Ip(host.parse::<Ipv6Addr>().ok()?.into()), port)
        } else {
            let (host, port) = match authority.split_once(':') {
                Some((host, port)) => (host, Some(parse_port(port)?)),
                None => (authority, None),
            };
            (parse_ipv4_or_dns(host)?, port)
        };
        Some(Self { host, port })
    }

    pub(crate) fn matches_socket(&self, local: SocketAddr, peer_is_loopback: bool) -> bool {
        self.http_port() == local.port()
            && match self.host {
                Host::Ip(ip) => ip.to_canonical() == local.ip().to_canonical(),
                Host::Dns(name) => {
                    peer_is_loopback
                        && local.ip().to_canonical().is_loopback()
                        && name.eq_ignore_ascii_case("localhost")
                }
            }
    }

    pub(crate) fn same_http_endpoint(&self, other: &Self) -> bool {
        self.same_host(other) && self.http_port() == other.http_port()
    }

    fn http_port(&self) -> u16 {
        self.port.unwrap_or(80)
    }

    fn same_host(&self, other: &Self) -> bool {
        match (&self.host, &other.host) {
            (Host::Ip(a), Host::Ip(b)) => a == b,
            (Host::Dns(a), Host::Dns(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        }
    }
}

fn parse_ipv4_or_dns(host: &str) -> Option<Host<'_>> {
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Some(Host::Ip(ip.into()));
    }
    let name = host.strip_suffix('.').unwrap_or(host);
    let last = name.rsplit('.').next()?;
    // Reject alternate IPv4 spellings that browser URL parsers can interpret
    // as IP addresses (e.g. 127.1, 2130706433 and 0x7f000001).
    if last.bytes().all(|byte| byte.is_ascii_digit())
        || last
            .strip_prefix("0x")
            .or_else(|| last.strip_prefix("0X"))
            .is_some_and(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return None;
    }
    (name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        }))
    .then_some(Host::Dns(host))
}

fn parse_port(port: &str) -> Option<u16> {
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    port.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_arguments_do_not_echo_values_or_consume_the_next_option() {
        for args in [
            &["--SECRET"][..],
            &["--mcp-host", "localhost"],
            &["--mcp-origin", "https://SECRET@example.com"],
            &["--rerank-api-key", "SECRET"],
            &[
                "--embedding-model",
                "SECRET",
                "--embedding-url",
                "--fusion",
                "edge",
            ],
        ] {
            let Err(error) = parse(args.iter().map(OsString::from)) else {
                panic!("invalid arguments were accepted");
            };
            assert!(!error.to_string().contains("SECRET"));
        }
    }

    #[test]
    fn rejects_ambiguous_http_authorities() {
        for authority in [
            "::1",
            "*",
            "localhost:*",
            "127.1",
            "host:65536",
            "[::1]:",
            "host:80:81",
            "host/path",
        ] {
            assert!(HttpAuthority::parse(authority).is_none());
        }
    }
}

extern crate ctrlc;
use bluer::{
    rfcomm::{SocketAddr, Stream},
    Address,
};
use chrono::Utc;
use clap::Parser;
use influxdb::{Client, InfluxDbWriteable, Timestamp};
use ini::Ini;
use simplelog::*;
use std::io::{self, Error, ErrorKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

/// secs between polling
pub const POLL_INTERVAL_SECS: f32 = 10.0;
/// secs between polling when car is in sleep mode or is not in range
pub const CAR_SLEEP_INTERVAL_SECS: f32 = 100.0;

const INIT: &[&str] = &["ATZ", "ATE0", "ATAL", "ATCP18", "ATFCSD300000", "ATSP6"];
const _EOM1: u8 = b'\r';
const EOM2: u8 = b'>';
const _EOM3: u8 = b'?';

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Simple daemon to read Renault Zoe basic parameters using
/// bluetooth dongle and save it in the InfluxDB database
#[derive(Parser, Debug)]
#[clap(version, about, long_about = None)]
struct Args {
    /// Enable debug info
    #[clap(short, long)]
    debug: bool,

    /// Config file path
    #[clap(short, long, parse(from_os_str), default_value = "/etc/canze-rs.conf")]
    config: std::path::PathBuf,
}

pub struct Parameter {
    name: String,
    desc: String,
    unit: Option<&'static str>,
    cmd: u32,
    reg_address: u16,
    reg_address2: u16,
    convert: Box<dyn Fn(u32) -> io::Result<f32>>,
}

impl Parameter {
    pub fn new(
        name: &'static str,
        desc: &'static str,
        unit: Option<&'static str>,
        cmd: u32,
        reg_address: u16,
        reg_address2: u16,
        convert: Box<dyn Fn(u32) -> io::Result<f32>>,
    ) -> Self {
        Self {
            name: String::from(name),
            desc: String::from(desc),
            unit,
            cmd,
            reg_address,
            reg_address2,
            convert,
        }
    }
}

fn create_params_table() -> Vec<Parameter> {
    vec![
        Parameter::new(
            "soc",
            "SOC",
            Some("%"),
            0x222002,
            0x7ec,
            0x7e4,
            Box::new(|val| {
                let x: f32 = (val - 0x20000) as f32 * 0.02;
                if x == 0.0 {
                    return Err(Error::new(ErrorKind::AddrNotAvailable, "CAN network down"));
                }
                Ok(x)
            }),
        ),
        Parameter::new(
            "odometer",
            "Total vehicle distance",
            Some("km"),
            0x222006,
            0x7ec,
            0x7e4,
            Box::new(|val| Ok(val as f32)),
        ),
        Parameter::new(
            "soh",
            "SOH",
            Some("%"),
            0x223206,
            0x7ec,
            0x7e4,
            Box::new(|val| Ok((val & 0xffff) as f32 * 0.05)),
        ),
        Parameter::new(
            "active_power",
            "Mains active power consumed",
            Some("W"),
            0x22504A,
            0x793,
            0x792,
            Box::new(|val| {
                Ok(((val & 0xffff)
                    .checked_sub(20000)
                    .ok_or(Error::new(ErrorKind::AddrNotAvailable, "substract error"))?)
                    as f32)
            }),
        ),
    ]
}

fn logging_init(debug: bool) {
    let conf = ConfigBuilder::new()
        .set_time_format("%F, %H:%M:%S%.3f".to_string())
        .set_write_log_enable_colors(true)
        .build();

    let mut loggers = vec![];

    let console_logger: Box<dyn SharedLogger> = TermLogger::new(
        if debug {
            LevelFilter::Debug
        } else {
            LevelFilter::Info
        },
        conf.clone(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    );
    loggers.push(console_logger);

    CombinedLogger::init(loggers).expect("Cannot initialize logging subsystem");
}

fn get_config_string(conf: Ini, option_name: &str, section: Option<&str>) -> io::Result<String> {
    conf.section(Some(section.unwrap_or("general").to_owned()))
        .and_then(|x| x.get(option_name).cloned())
        .ok_or(Error::new(
            ErrorKind::Other,
            format!("No config entry for: `{option_name}`"),
        ))
}

pub async fn send_cmd(stream: &mut Stream, cmd: String) -> io::Result<Option<Vec<u8>>> {
    let mut buffer = vec![0u8; 512];
    let mut output_cmd: Vec<u8> = vec![];
    let out: Option<Vec<u8>>;

    output_cmd.extend(cmd.as_bytes());
    output_cmd.push(b'\r');
    debug!("write: {}", String::from_utf8_lossy(&output_cmd));
    if let Err(e) = stream.write_all(&output_cmd).await {
        error!("write error: {:?}", e);
        return Err(e.into());
    }

    let mut packet = BufReader::new(stream);
    let retval = packet.read_until(EOM2, &mut buffer);
    match timeout(Duration::from_secs_f32(5.0), retval).await {
        Ok(res) => match res {
            Ok(len) => {
                if len == 0 {
                    error!("file read error: 0 bytes");
                    return Err(Error::new(ErrorKind::Other, "0 bytes read"));
                }
                out = Some(buffer.clone());
                trace!("Response: {:?}", buffer);
                let ascii = String::from_utf8_lossy(&buffer);
                debug!("Response ASCII (len={}): {}", len, ascii);
                if ascii.contains("NO DATA") {
                    return Err(Error::new(ErrorKind::Other, "no data"));
                }
                if ascii.contains("7F 22 12") {
                    return Err(Error::new(ErrorKind::Other, "Service Not Supported"));
                }
            }
            Err(e) => {
                error!("file read error: {}", e);
                return Err(e.into());
            }
        },
        Err(e) => {
            error!("response timeout: {}", e);
            return Err(e.into());
        }
    }

    Ok(out)
}

async fn influx_save_param(client: influxdb::Client, name: &str, val: f32) -> Result<()> {
    // construct a write query
    let mut write_query = Timestamp::from(Utc::now()).into_query("params");
    write_query = write_query.add_field(name, val);

    // send query to influxdb
    let write_result = client.query(&write_query).await;
    match write_result {
        Ok(msg) => {
            debug!("influxdb write success: {:?}", msg);
        }
        Err(e) => {
            error!("influxdb write error: {:?}", e);
        }
    }

    Ok(())
}

pub async fn get_param(
    stream: &mut Stream,
    p: &Parameter,
    client: influxdb::Client,
) -> io::Result<()> {
    let cmd = format!("ATSH{:02x}\r", p.reg_address2);
    send_cmd(stream, cmd).await?;
    let cmd = format!("ATCRA{:02x}\r", p.reg_address);
    send_cmd(stream, cmd).await?;
    let cmd = format!("ATFCSH{:02x}\r", p.reg_address2);
    send_cmd(stream, cmd).await?;
    let cmd = format!("10C0\r");
    let _ = send_cmd(stream, cmd).await;
    let cmd = format!("{:02x}\r", p.cmd);
    let out = send_cmd(stream, cmd).await?.unwrap();
    let mut raw_string = String::from_utf8_lossy(&out);
    raw_string = raw_string
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .into();
    debug!("got response for {}: {}", p.name, raw_string);

    //get an u32 value from a response hex string
    let val = u32::from_str_radix(&raw_string[raw_string.len() - 6..raw_string.len()], 16).unwrap();
    //use an associated parameter converter for a value
    let converted = (p.convert)(val)?;

    info!(
        "{} ({}): {} {}",
        p.desc,
        p.name,
        converted,
        p.unit.unwrap_or_default()
    );
    let _ = influx_save_param(client, &p.name, converted).await;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    logging_init(args.debug);
    info!("<b><blue>canze-rs</> started");
    info!("Using config file: <b><blue>{:?}</>", args.config);
    let conf = match Ini::load_from_file(args.config) {
        Ok(c) => c,
        Err(e) => {
            error!("Cannot open config file: {}", e);
            return Ok(());
        }
    };
    let mac = get_config_string(conf.clone(), "mac", None)?;
    let influxdb_url = get_config_string(conf.clone(), "url", Some("influxdb"))?;
    let influxdb_db = get_config_string(conf.clone(), "db", Some("influxdb"))?;

    //parse target mac address for bluetooth
    let target_addr: Address = mac.parse().expect("invalid address");
    let target_sa = SocketAddr::new(target_addr, 1u8);

    //Ctrl-C / SIGTERM support
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    let params = create_params_table();
    let mut poll_interval = Instant::now();
    let client = Client::new(influxdb_url, influxdb_db);

    'connect: loop {
        if !running.load(Ordering::SeqCst) {
            info!("🛑 Ctrl-C or SIGTERM signal detected, exiting...");
            break;
        }

        tokio::time::sleep(Duration::from_secs(10)).await;
        info!("Connecting to: {:?}", &target_sa);
        let res = Stream::connect(target_sa).await;
        let mut stream = if let Ok(s) = res {
            s
        } else {
            info!("Cannot connect (BT dongle not in range?)");
            continue;
        };

        // the following code is a workaround for a problem described here:
        // https://github.com/bluez/bluer/discussions/130#discussioncomment-8845113
        debug!("Local address before: {:?}", stream.as_ref().local_addr()?);
        let mut i = 0;
        while stream.as_ref().local_addr()?.addr == bluer::Address::any() {
            debug!("Waiting for local address...");
            tokio::time::sleep(Duration::from_secs(1)).await;
            i += 1;
            if i > 5 {
                break;
            }
        }

        info!("Local address: {:?}", stream.as_ref().local_addr()?);
        //info!("Remote address: {:?}", stream.peer_addr()?);
        info!("Security: {:?}", stream.as_ref().security()?);

        info!("connected, poll interval: {}s", POLL_INTERVAL_SECS);

        for s in INIT {
            if let Err(_) = send_cmd(&mut stream, s.to_string()).await {
                info!("INIT error, reconnect");
                continue 'connect;
            }
        }

        'inner: loop {
            if !running.load(Ordering::SeqCst) {
                continue 'connect;
            }

            if poll_interval.elapsed() > Duration::from_secs(0) {
                poll_interval = Instant::now() + Duration::from_secs_f32(POLL_INTERVAL_SECS);

                for p in &params {
                    debug!("Trying to obtain: {} ({})", p.desc, p.name);
                    if let Err(e) = get_param(&mut stream, p, client.clone()).await {
                        info!("GET PARAM error for: {}: {:?}", p.name, e);
                        if e.kind() == std::io::ErrorKind::AddrNotAvailable {
                            info!("CAN network down / car is sleeping... waiting 100s");
                            poll_interval =
                                Instant::now() + Duration::from_secs_f32(CAR_SLEEP_INTERVAL_SECS);
                            continue 'inner;
                        }
                        if e.kind() == std::io::ErrorKind::BrokenPipe
                            || e.kind() == std::io::ErrorKind::TimedOut
                            || e.kind() == std::io::ErrorKind::NotConnected
                        {
                            info!("Broken pipe/TimedOut/NotConnected detected ... trying to reconnect");
                            continue 'connect;
                        }
                    }
                }
                debug!("Got all params, sleeping 10 secs for next cycle");
            }

            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    Ok(())
}

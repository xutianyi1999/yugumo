#[macro_use]
extern crate log;

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

use log::LevelFilter;
use log4rs::append::console::ConsoleAppender;
use log4rs::config::{Appender, Root};
use log4rs::encode::pattern::PatternEncoder;
use log4rs::Config;
use socket2::TcpKeepalive;
use tokio::net::{TcpListener, TcpStream};

fn logger_init() {
    let stdout = ConsoleAppender::builder()
        .encoder(Box::new(PatternEncoder::new(
            "[Console] {d(%Y-%m-%d %H:%M:%S)} - {l} - {m}{n}",
        )))
        .build();

    let config = Config::builder()
        .appender(Appender::builder().build("stdout", Box::new(stdout)))
        .build(
            Root::builder().appender("stdout").build(
                LevelFilter::from_str(&std::env::var("RUST_LOG").unwrap_or(String::from("INFO")))
                    .unwrap(),
            ),
        )
        .unwrap();

    log4rs::init_config(config).unwrap();
}

fn zip<T1, T2, E1, E2>(a: Result<T1, E1>, b: Result<T2, E2>) -> Option<(T1, T2)> {
    match (a, b) {
        (Ok(a), Ok(b)) => Some((a, b)),
        _ => None,
    }
}

pub trait SocketExt {
    fn set_keepalive(&self) -> io::Result<()>;
}

const TCP_KEEPALIVE: TcpKeepalive = TcpKeepalive::new().with_time(Duration::from_secs(120));

macro_rules! build_socket_ext {
    ($type:path) => {
        impl<T: $type> SocketExt for T {
            fn set_keepalive(&self) -> io::Result<()> {
                let sock_ref = socket2::SockRef::from(self);
                sock_ref.set_tcp_keepalive(&TCP_KEEPALIVE)
            }
        }
    };
}

#[cfg(windows)]
build_socket_ext!(std::os::windows::io::AsRawSocket);

#[cfg(unix)]
build_socket_ext!(std::os::unix::io::AsRawFd);

#[tokio::main]
async fn main() {
    logger_init();
    let mut args = std::env::args();
    args.next();

    let args: Vec<String> = args.collect();

    let map: HashMap<SocketAddr, SocketAddr> = args
        .iter()
        .map(|str| str.split_once("->"))
        .filter(Option::is_some)
        .map(Option::unwrap)
        .map(|(a, b)| zip(SocketAddr::from_str(a), SocketAddr::from_str(b)))
        .filter(Option::is_some)
        .map(Option::unwrap)
        .collect();

    let mut serves = Vec::with_capacity(map.len());

    for (source, dest) in map {
        serves.push(tokio::spawn(async move {
            let fut = async move {
                let listener = TcpListener::bind(source).await?;
                info!("{} -> {} serve start", source, dest);

                loop {
                    let (mut source_stream, peer_addr) = listener.accept().await?;

                    tokio::spawn(async move {
                        debug!("{} forward to {}", peer_addr, dest);

                        let res = async move {
                            source_stream.set_keepalive()?;

                            let mut dest_stream = TcpStream::connect(dest).await?;
                            dest_stream.set_keepalive()?;
                            tokio::io::copy_bidirectional(&mut source_stream, &mut dest_stream).await?;
                            Result::<(), io::Error>::Ok(())
                        }
                        .await;

                        if let Err(e) = res {
                            error!("{} forward error: {:?}", peer_addr, e)
                        }
                    });
                }
            };

            let res: Result<(), io::Error> = fut.await;

            if let Err(e) = res {
                error!("{} serve error: {}", source, e)
            }
        }));
    }

    for h in serves {
        if let Err(e) = h.await {
            error!("Process error: {}", e)
        }
    }
}

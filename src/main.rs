#[macro_use]
extern crate log;

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::ops::Deref;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use arc_swap::{ArcSwap, Cache};
use chrono::Utc;
use clap::{arg, Parser};
use log4rs::append::console::ConsoleAppender;
use log4rs::config::{Appender, Root};
use log4rs::Config;
use log4rs::encode::pattern::PatternEncoder;
use log::LevelFilter;
use socket2::TcpKeepalive;
use tokio::net::{TcpListener, TcpStream, UdpSocket};

fn logger_init() {
    let stdout = ConsoleAppender::builder()
        .encoder(Box::new(PatternEncoder::new(
            "[{d(%Y-%m-%d %H:%M:%S)}] {h({l})} {t} - {m}{n}"
        )))
        .build();

    let config = Config::builder()
        .appender(Appender::builder().build("stdout", Box::new(stdout)))
        .build(
            Root::builder().appender("stdout").build(
                LevelFilter::from_str(std::env::var("YUGUMO_LOG").as_deref().unwrap_or("INFO"))
                    .unwrap(),
            ),
        )
        .unwrap();

    log4rs::init_config(config).unwrap();
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
build_socket_ext!(std::os::windows::io::AsSocket);

#[cfg(unix)]
build_socket_ext!(std::os::unix::io::AsFd);

async fn udp_handler(
    bind: &str,
    to: &str,
) -> io::Result<()> {
    // (peer, to socket, update timestamp)
    let mapping: &'static ArcSwap<Vec<Arc<(SocketAddr, UdpSocket, AtomicI64)>>> = Box::leak(Box::new(ArcSwap::from_pointee(Vec::new())));
    let local_socket: &'static UdpSocket = Box::leak(Box::new(UdpSocket::bind(bind).await?));
    info!("{} -> {} udp serve start", bind, to);

    let mut buff = vec![0u8; 65536];
    let mut mapping_cache = Cache::new(mapping);

    loop {
        let (len, peer) = local_socket.recv_from(&mut buff).await?;
        let snap = mapping_cache.load();

        let item = snap.binary_search_by_key(&peer, |v| (**v).0)
            .ok()
            .map(|i| &*snap.deref()[i]);

        let insert_item;

        let (_, to_socket, update_time) = match item {
            None => {
                let to = tokio::net::lookup_host(to).await?.next().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "parse dest address failure"))?;

                let bind_addr = match to {
                    SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
                    SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
                };

                let to_socket = UdpSocket::bind(bind_addr).await?;
                to_socket.connect(to).await?;

                insert_item = Arc::new((peer, to_socket, AtomicI64::new(Utc::now().timestamp())));

                mapping.rcu(|v| {
                    let mut tmp = (**v).clone();

                    match tmp.binary_search_by_key(&peer, |v| (**v).0) {
                        Ok(_) => unreachable!(),
                        Err(i) => tmp.insert(i, insert_item.clone()),
                    }
                    tmp
                });

                tokio::spawn({
                    let insert_item = insert_item.clone();

                    async move {
                        let (_, to_socket, update_time) = &*insert_item;
                        let mut buff = vec![0u8; 65536];

                        let fut1 = async {
                            loop {
                                let len = to_socket.recv(&mut buff).await?;
                                debug!("recv from {} to {}", to, peer);
                                local_socket.send_to(&buff[..len], peer).await?;
                                update_time.store(Utc::now().timestamp(), Ordering::Relaxed);
                            }
                        };

                        let fut2 = async {
                            loop {
                                tokio::time::sleep(Duration::from_secs(5)).await;

                                if Utc::now().timestamp() - update_time.load(Ordering::Relaxed) > 300 {
                                    return;
                                }
                            }
                        };

                        let res: io::Result<()> = tokio::select! {
                            res = fut1 => res,
                            _ = fut2 => Ok(())
                        };

                        if let Err(e) = res {
                            error!("child udp handler error: {}", e);
                        }

                        mapping.rcu(|v| {
                            let mut tmp = (**v).clone();

                            match tmp.binary_search_by_key(&peer, |v| (**v).0) {
                                Ok(i) => tmp.remove(i),
                                Err(_) => unreachable!(),
                            };
                            tmp
                        });
                    }
                });

                &*insert_item
            }
            Some(v) => v
        };

        to_socket.send(&buff[..len]).await?;
        update_time.store(Utc::now().timestamp(), Ordering::Relaxed);
    }
}

async fn tcp_handler(
    bind: &str,
    to: &'static str,
) -> io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    info!("{} -> {} tcp serve start", bind, to);

    loop {
        let (mut source_stream, peer_addr) = listener.accept().await?;

        tokio::spawn(async move {
            debug!("{} forward to {}", peer_addr, to);

            let res = async move {
                source_stream.set_keepalive()?;

                let mut dest_stream = TcpStream::connect(to).await?;
                dest_stream.set_keepalive()?;
                tokio::io::copy_bidirectional(&mut source_stream, &mut dest_stream).await?;
                Result::<(), io::Error>::Ok(())
            }.await;

            if let Err(e) = res {
                error!("{} forward error: {}", peer_addr, e)
            }
        });
    }
}

#[derive(Parser)]
#[command(version)]
struct Args {
    /// example: "0.0.0.0:80->google.com:80"
    #[arg(short, long)]
    tcp: Vec<String>,
    /// example: "0.0.0.0:53->8.8.8.8:53"
    #[arg(short, long)]
    udp: Vec<String>,
}

fn main() -> io::Result<()> {
    let args: Args = Args::parse();
    logger_init();

    let rt = tokio::runtime::Runtime::new()?;

    rt.block_on(async move {
        let mut serves = Vec::new();

        for v in args.tcp {
            let v: &'static str = &**Box::leak(Box::new(v));

            if let Some((bind, to)) = v.split_once("->") {
                let fut = tokio::spawn(async move {
                    if let Err(e) = tcp_handler(bind, to).await {
                        error!("TCP handler error: {}", e)
                    }
                });
                serves.push(fut);
            }
        }

        for v in args.udp {
            let v: &'static str = &**Box::leak(Box::new(v));

            if let Some((bind, to)) = v.split_once("->") {
                let fut = tokio::spawn(async move {
                    if let Err(e) = udp_handler(bind, to).await {
                        error!("UDP handler error: {}", e)
                    }
                });
                serves.push(fut);
            }
        }

        for h in serves {
            if let Err(e) = h.await {
                error!("servers error: {}", e)
            }
        }
    });
    Ok(())
}

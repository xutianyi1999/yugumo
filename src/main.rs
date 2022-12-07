#[macro_use]
extern crate log;

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::ops::Deref;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use ahash::{HashMap, HashMapExt};
use chrono::Utc;
use clap::{arg, Parser};
use log4rs::append::console::ConsoleAppender;
use log4rs::config::{Appender, Root};
use log4rs::Config;
use log4rs::encode::pattern::PatternEncoder;
use log::LevelFilter;
use parking_lot::RwLock;
use socket2::TcpKeepalive;
use tokio::net::{TcpListener, TcpStream, UdpSocket};

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

struct ConcurrentCache<V> {
    version: AtomicUsize,
    inner: RwLock<V>,
}

impl<V: Clone> ConcurrentCache<V> {
    fn new(v: V) -> Self {
        ConcurrentCache {
            version: AtomicUsize::new(0),
            inner: RwLock::new(v),
        }
    }

    #[allow(unused)]
    fn try_update<R, F: FnOnce(&mut V) -> R>(&self, f: F) -> Result<R, F> {
        match self.inner.try_write() {
            Some(mut v) => {
                self.version.fetch_add(1, Ordering::Relaxed);
                Ok(f(&mut *v))
            }
            None => return Err(f),
        }
    }

    fn update<R, F: FnOnce(&mut V) -> R>(&self, f: F) -> R {
        let mut guard = self.inner.write();
        self.version.fetch_add(1, Ordering::Relaxed);
        f(&mut *guard)
    }

    #[allow(unused)]
    fn try_load(&self) -> Option<V> {
        self.inner.try_read().map(|v| v.clone())
    }

    fn load(&self) -> V {
        self.inner.read().clone()
    }

    fn guard(&self) -> Guard<V> {
        let version = self.version.load(Ordering::Acquire);
        let v = self.load();

        Guard {
            cache: self,
            v,
            version,
        }
    }
}

struct Guard<'a, V> {
    cache: &'a ConcurrentCache<V>,
    v: V,
    version: usize,
}

impl<V: Clone> Guard<'_, V> {
    fn sync(&mut self) {
        let cache_version = self.cache.version.load(Ordering::Acquire);

        if self.version == cache_version {
            return;
        }

        self.v = self.cache.load();
        self.version = cache_version;
    }
}

impl<V> Deref for Guard<'_, V> {
    type Target = V;

    fn deref(&self) -> &Self::Target {
        &self.v
    }
}

async fn clock_task(local_clock: &'static AtomicI64) {
    loop {
        local_clock.store(Utc::now().timestamp(), Ordering::Relaxed);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn udp_handler(
    bind: &'static str,
    to: &'static str,
    mapping: &'static ConcurrentCache<HashMap<SocketAddr, Arc<(UdpSocket, AtomicI64)>>>,
    clock: &'static AtomicI64,
) -> io::Result<()> {
    let socket: &'static UdpSocket = Box::leak(Box::new(UdpSocket::bind(bind).await?));
    info!("{} -> {} udp serve start", bind, to);

    let mut buff = vec![0u8; 65536];
    let mut guard = mapping.guard();

    loop {
        let (len, peer) = socket.recv_from(&mut buff).await?;

        let (dst_socket, update_time) = loop {
            guard.sync();

            match guard.get(&peer) {
                None => {
                    let to = tokio::net::lookup_host(to).await?.next().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Parse dest address failure"))?;

                    let bind_addr = match to {
                        SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
                        SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
                    };

                    let new_socket = UdpSocket::bind(bind_addr).await?;

                    mapping.update(|map| {
                        if !map.contains_key(&peer) {
                            let v = Arc::new((new_socket, AtomicI64::new(clock.load(Ordering::Relaxed))));
                            map.insert(peer, v.clone());

                            tokio::spawn(async move {
                                let (dst_socket, timestamp) = &*v;
                                let mut buff = vec![0u8; 65536];

                                let fut1 = async {
                                    loop {
                                        let len = dst_socket.recv(&mut buff).await?;
                                        socket.send_to(&buff[..len], peer).await?;
                                        timestamp.store(clock.load(Ordering::Relaxed), Ordering::Relaxed);
                                    }
                                };

                                let fut2 = async {
                                    loop {
                                        tokio::time::sleep(Duration::from_secs(5)).await;

                                        if clock.load(Ordering::Relaxed) - timestamp.load(Ordering::Relaxed) > 300 {
                                            mapping.update(|m| m.remove(&peer));
                                            return;
                                        }
                                    }
                                };

                                let res: io::Result<()> = tokio::select! {
                                    res = fut1 => res,
                                    _ = fut2 => Ok(())
                                };

                                if let Err(e) = res {
                                    error!("Child udp handler error: {}", e);
                                }
                            });
                        }
                    })
                }
                Some(v) => break &**v
            };
        };

        dst_socket.send_to(&buff[..len], to).await?;
        update_time.store(clock.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

async fn tcp_handler(
    bind: &'static str,
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
                error!("{} forward error: {:?}", peer_addr, e)
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

        let mapping = &*Box::leak(Box::new(ConcurrentCache::new(HashMap::new())));
        let clock = &*Box::leak(Box::new(AtomicI64::new(Utc::now().timestamp())));

        for v in args.udp {
            let v: &'static str = &**Box::leak(Box::new(v));
            if let Some((bind, to)) = v.split_once("->") {
                let fut = tokio::spawn(async move {
                    if let Err(e) = udp_handler(bind, to, mapping, clock).await {
                        error!("UDP handler error: {}", e)
                    }
                });
                serves.push(fut);
            }
        }

        serves.push(tokio::spawn(clock_task(clock)));

        for h in serves {
            if let Err(e) = h.await {
                error!("Process error: {}", e)
            }
        }
    });
    Ok(())
}

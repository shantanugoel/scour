//! SOCKS5 forwarder that opens only IPv4 connections.
//!
//! Chrome runs with a proxy on its command line. With no configured proxy, this
//! forwarder is that proxy, so an engine name that resolves to IPv6 still gets an
//! IPv4 connection. The forwarder carries bytes only; TLS starts in Chrome.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

pub struct Forwarder {
    pub port: u16,
    // Dropping aborts the accept task, which drops its connection tasks.
    _tasks: JoinSet<()>,
}

pub async fn start() -> io::Result<Forwarder> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let mut tasks = JoinSet::new();
    tasks.spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((client, _)) = accepted else {
                        break;
                    };
                    connections.spawn(handle(client));
                }
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    });
    Ok(Forwarder {
        port,
        _tasks: tasks,
    })
}

async fn handle(mut client: TcpStream) -> io::Result<()> {
    let mut greeting = [0u8; 2];
    client.read_exact(&mut greeting).await?;
    if greeting[0] != 5 {
        return Ok(());
    }
    let mut methods = vec![0u8; greeting[1] as usize];
    client.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        client.write_all(&[5, 0xff]).await?;
        return Ok(());
    }
    client.write_all(&[5, 0]).await?;

    let mut head = [0u8; 4];
    client.read_exact(&mut head).await?;
    if head[0] != 5 {
        return Ok(());
    }
    if head[1] != 1 {
        reply(&mut client, 7).await?;
        return Ok(());
    }
    let dest = match head[3] {
        1 => {
            let mut addr = [0u8; 4];
            client.read_exact(&mut addr).await?;
            let mut port = [0u8; 2];
            client.read_exact(&mut port).await?;
            SocketAddr::from((Ipv4Addr::from(addr), u16::from_be_bytes(port)))
        }
        3 => {
            let mut len = [0u8; 1];
            client.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            client.read_exact(&mut name).await?;
            let mut port = [0u8; 2];
            client.read_exact(&mut port).await?;
            let Ok(host) = std::str::from_utf8(&name) else {
                reply(&mut client, 8).await?;
                return Ok(());
            };
            match ipv4_lookup(host, u16::from_be_bytes(port)).await {
                Some(addr) => addr,
                None => {
                    reply(&mut client, 4).await?;
                    return Ok(());
                }
            }
        }
        4 => {
            let mut ignore = [0u8; 18];
            let _ = client.read_exact(&mut ignore).await;
            reply(&mut client, 8).await?;
            return Ok(());
        }
        _ => {
            reply(&mut client, 8).await?;
            return Ok(());
        }
    };
    let mut upstream = match TcpStream::connect(dest).await {
        Ok(stream) => stream,
        Err(_) => {
            reply(&mut client, 5).await?;
            return Ok(());
        }
    };
    reply(&mut client, 0).await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

async fn ipv4_lookup(host: &str, port: u16) -> Option<SocketAddr> {
    let addrs = tokio::net::lookup_host((host, port)).await.ok()?;
    addrs.into_iter().find(|addr| addr.is_ipv4())
}

async fn reply(client: &mut TcpStream, status: u8) -> io::Result<()> {
    client.write_all(&[5, status, 0, 1, 0, 0, 0, 0, 0, 0]).await
}

use std::{net::Ipv4Addr, sync::Arc};

use anyctx::AnyCtx;
use futures_util::AsyncReadExt as _;
use geph5_misc_rpc::{
    exit::{ClientCryptHello, ClientExitCryptPipe, ClientHello, ExitHello, ExitHelloInner},
    read_prepend_length, write_prepend_length,
};
use picomux::PicoMux;
use sillad::{dialer::Dialer as _, listener::Listener as _, Pipe};
use smol::future::FutureExt as _;
use socksv5::v5::{
    read_handshake, read_request, write_auth_method, write_request_status, SocksV5AuthMethod,
    SocksV5Host, SocksV5RequestStatus,
};
use stdcode::StdcodeSerializeExt;

use super::Config;

pub async fn client_inner(ctx: AnyCtx<Config>) -> anyhow::Result<()> {
    let raw_dialer = ctx.init().exit_constraint.dialer().await?;
    let raw_pipe = raw_dialer.dial().await?;
    tracing::debug!("raw dialer done");
    let authed_pipe = client_auth(raw_pipe).await?;
    tracing::debug!("authentication done, starting mux system");
    let (read, write) = authed_pipe.split();
    let mux = Arc::new(PicoMux::new(read, write));
    let mut listener = sillad::tcp::TcpListener::bind(ctx.init().socks5_listen).await?;

    let (send_stop, mut recv_stop) = tachyonix::channel(1);
    // run a socks5 loop
    async {
        let err: std::io::Error = recv_stop.recv().await?;
        Err(err.into())
    }
    .race(async {
        loop {
            let client = listener.accept().await?;
            let mux = mux.clone();
            let send_stop = send_stop.clone();

            smolscale::spawn(async move {
                let (mut read_client, mut write_client) = client.split();
                let _handshake = read_handshake(&mut read_client).await?;
                write_auth_method(&mut write_client, SocksV5AuthMethod::Noauth).await?;
                let request = read_request(&mut read_client).await?;
                let port = request.port;
                let domain: String = match &request.host {
                    SocksV5Host::Domain(dom) => String::from_utf8_lossy(dom).parse()?,
                    SocksV5Host::Ipv4(v4) => {
                        let v4addr = Ipv4Addr::new(v4[0], v4[1], v4[2], v4[3]);
                        v4addr.to_string()
                    }
                    _ => anyhow::bail!("IPv6 not supported"),
                };
                let remote_addr = format!("{domain}:{port}");
                tracing::debug!(remote_addr = display(&remote_addr), "connecting to remote");
                let stream = mux.open(remote_addr.as_bytes()).await;
                match stream {
                    Ok(stream) => {
                        let (read, write) = stream.split();
                        write_request_status(
                            &mut write_client,
                            SocksV5RequestStatus::Success,
                            request.host,
                            port,
                        )
                        .await?;
                        smol::io::copy(read, write_client)
                            .race(smol::io::copy(read_client, write))
                            .await?;
                        Ok(())
                    }
                    Err(err) => {
                        let _ = send_stop.try_send(err);
                        Ok(())
                    }
                }
            })
            .detach();
        }
    })
    .await
}

async fn client_auth(mut pipe: impl Pipe) -> anyhow::Result<impl Pipe> {
    match pipe.shared_secret() {
        Some(_) => todo!(),
        None => {
            tracing::debug!("requiring full authentication");
            let my_esk = x25519_dalek::EphemeralSecret::random_from_rng(rand::thread_rng());
            let client_hello = ClientHello {
                credentials: Default::default(), // no authentication support yet
                crypt_hello: ClientCryptHello::X25519((&my_esk).into()),
            };
            write_prepend_length(&client_hello.stdcode(), &mut pipe).await?;
            let exit_hello: ExitHello =
                stdcode::deserialize(&read_prepend_length(&mut pipe).await?)?;
            match exit_hello.inner {
                ExitHelloInner::Reject(reason) => {
                    anyhow::bail!("exit rejected our authentication attempt: {reason}")
                }
                ExitHelloInner::SharedSecretResponse(_) => anyhow::bail!(
                    "exit sent a shared-secret response to our full authentication request"
                ),
                ExitHelloInner::X25519(their_epk) => {
                    let shared_secret = my_esk.diffie_hellman(&their_epk);
                    let read_key = blake3::derive_key("e2c", shared_secret.as_bytes());
                    let write_key = blake3::derive_key("c2e", shared_secret.as_bytes());
                    Ok(ClientExitCryptPipe::new(pipe, read_key, write_key))
                }
            }
        }
    }
}

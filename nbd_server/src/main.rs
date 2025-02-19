// Copyright 2021 Oxide Computer Company
use std::net::SocketAddrV4;
use std::sync::Arc;

use anyhow::{bail, Result};
use structopt::StructOpt;
use tokio::runtime::Builder;

use crucible::*;

use nbd::server::{handshake, transmission, Export};
use std::net::{TcpListener, TcpStream as NetTcpStream};

/*
 * NBD server commands translate through the CruciblePseudoFile and turn
 * into Guest work ops.
 */

fn handle_nbd_client(
    cpf: &mut crucible::CruciblePseudoFile,
    mut stream: NetTcpStream,
) -> Result<()> {
    let e = Export {
        size: cpf.sz(),
        readonly: false,
        ..Default::default()
    };
    handshake(&mut stream, &e)?;
    transmission(&mut stream, cpf)?;
    Ok(())
}

#[derive(Debug, StructOpt)]
#[structopt(about = "volume-side storage component")]
pub struct Opt {
    #[structopt(short, long, default_value = "127.0.0.1:9000")]
    target: Vec<SocketAddrV4>,

    #[structopt(short, long)]
    key: Option<String>,
}

pub fn opts() -> Result<Opt> {
    let opt: Opt = Opt::from_args();
    println!("raw options: {:?}", opt);

    if opt.target.is_empty() {
        bail!("must specify at least one --target");
    }

    Ok(opt)
}

fn main() -> Result<()> {
    let opt = opts()?;
    let crucible_opts = CrucibleOpts {
        target: opt.target,
        lossy: false,
        key: opt.key,
    };

    /*
     * Crucible needs a runtime as it will create several async tasks to
     * handle adding new IOs, communication with the three downstairs
     * instances, and completing IOs.
     */
    let runtime = Builder::new_multi_thread()
        .worker_threads(10)
        .thread_name("crucible-tokio")
        .enable_all()
        .build()
        .unwrap();

    /*
     * The structure we use to send work from outside crucible into the
     * Upstairs main task.
     * We create this here instead of inside up_main() so we can use
     * the methods provided by guest to interact with Crucible.
     */
    let guest = Arc::new(Guest::new());

    runtime.spawn(up_main(crucible_opts, guest.clone()));
    println!("Crucible runtime is spawned");

    // NBD server

    let listener = TcpListener::bind("127.0.0.1:10809").unwrap();
    let mut cpf = crucible::CruciblePseudoFile::from_guest(guest)?;

    cpf.activate()?;

    // sent to NBD client during handshake through Export struct
    println!("NBD advertised size as {} bytes", cpf.sz());

    for stream in listener.incoming() {
        println!("waiting on nbd traffic");
        match stream {
            Ok(stream) => match handle_nbd_client(&mut cpf, stream) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("handle_nbd_client error: {}", e);
                }
            },
            Err(_) => {
                println!("Error");
            }
        }
    }

    Ok(())
}

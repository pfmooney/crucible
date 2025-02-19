// Copyright 2021 Oxide Computer Company
#![feature(with_options)]

use std::net::SocketAddrV4;
use std::sync::Arc;

use anyhow::{bail, Result};
use structopt::StructOpt;
use tokio::runtime::Builder;

use crucible::*;

use std::io::{Read, Seek, SeekFrom, Write};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

// https://stackoverflow.com/questions/29504514/whats-the-best-way-to-compare-2-vectors-or-strings-element-by-element
fn do_vecs_match<T: PartialEq>(a: &[T], b: &[T]) -> bool {
    let matching = a.iter().zip(b.iter()).filter(|&(a, b)| a == b).count();
    matching == a.len() && matching == b.len()
}

#[derive(Debug, StructOpt)]
#[structopt(about = "volume-side storage component")]
pub struct Opt {
    #[structopt(short, long, default_value = "127.0.0.1:9000")]
    target: Vec<SocketAddrV4>,

    /*
     * Verify that writes don't extend before or after the actual location.
     */
    #[structopt(short, long)]
    verify_isolation: bool,

    #[structopt(long)]
    tracing_endpoint: Option<String>,

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

    if let Some(tracing_endpoint) = opt.tracing_endpoint {
        let tracer = opentelemetry_jaeger::new_pipeline()
            .with_agent_endpoint(tracing_endpoint)
            .with_service_name("crucible-hammer")
            .install_simple()
            .expect("Error initializing Jaeger exporter");

        let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);

        tracing_subscriber::registry()
            .with(telemetry)
            .try_init()
            .expect("Error init tracing subscriber");

        println!("Set up tracing!");
    }

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

    // Create 5 CruciblePseudoFiles to test activation handoff.
    let mut cpfs: Vec<crucible::CruciblePseudoFile> = Vec::with_capacity(5);

    for _ in 0..5 {
        /*
         * The structure we use to send work from outside crucible into the
         * Upstairs main task.
         * We create this here instead of inside up_main() so we can use
         * the methods provided by guest to interact with Crucible.
         */
        let guest = Arc::new(Guest::new());

        runtime.spawn(up_main(crucible_opts.clone(), guest.clone()));
        println!("Crucible runtime is spawned");

        cpfs.push(crucible::CruciblePseudoFile::from_guest(guest)?);
    }

    use rand::Rng;
    let mut rng = rand::thread_rng();

    let rounds = 1500;
    let handoff_amount = rounds / 5;
    let mut cpf_idx = 0;

    println!("Handing off to CPF {}", cpf_idx);
    cpfs[cpf_idx].activate()?;
    println!(
        "Handed off to CPF {} {:?}",
        cpf_idx,
        cpfs[cpf_idx].upstairs_uuid()
    );

    if opt.verify_isolation {
        println!("clearing...");

        let cpf = &mut cpfs[0];
        cpf.seek(SeekFrom::Start(0))?;

        let bs = cpf.block_size();
        let sz = cpf.sz();

        for _ in 0..(sz / bs) {
            cpf.write_all(&vec![0; bs as usize])?;
        }
    }

    for idx in 0..rounds {
        let cpf = if idx / handoff_amount != cpf_idx {
            cpf_idx = idx / handoff_amount;
            assert!(cpf_idx != 0);

            println!("Handing off to CPF {}", cpf_idx);

            let cpf = &mut cpfs[cpf_idx];
            cpf.activate()?;

            println!("Handed off to CPF {} {:?}", cpf_idx, cpf.upstairs_uuid());

            cpf
        } else {
            &mut cpfs[cpf_idx]
        };

        let sz = cpf.sz();

        let mut offset: u64 = rng.gen::<u64>() % sz;
        let mut bsz: usize = rng.gen::<usize>() % 4096;

        while ((offset + bsz as u64) > sz) || (bsz == 0) {
            offset = rng.gen::<u64>() % sz;
            bsz = rng.gen::<usize>() % 4096;
        }

        // println!("testing {}: offset {} sz {}", idx, offset, bsz);

        let vec: Vec<u8> = (0..bsz)
            .map(|_| rng.sample(rand::distributions::Standard))
            .collect();

        let mut vec2 = vec![0; bsz];

        cpf.seek(SeekFrom::Start(offset))?;
        cpf.write_all(&vec[..])?;

        cpf.seek(SeekFrom::Start(offset))?;
        cpf.read_exact(&mut vec2[..])?;

        if !do_vecs_match(&vec, &vec2) {
            println!("offset {} bsz {}", offset, bsz);
            println!("vec : {:?}", vec);
            println!("vec2: {:?}", vec2);

            assert_eq!(vec.len(), vec2.len());

            for i in 0..vec.len() {
                if vec[i] != vec2[i] {
                    println!("vec offset {}: {} != {}", i, vec[i], vec2[i]);
                } else {
                    println!("vec offset {} ok", i);
                }
            }

            bail!("vec != vec2");
        }

        if opt.verify_isolation {
            // Read back every byte not written to to make sure it's zero
            cpf.seek(SeekFrom::Start(0))?;

            // read from 0 -> offset
            let mut verify_vec: Vec<u8> = vec![0; offset as usize];
            cpf.read_exact(&mut verify_vec[..])?;

            for i in 0..offset {
                if verify_vec[i as usize] != 0 {
                    bail!("Not isolated! non-zero byte at {}", i);
                }
            }

            // read from (offset + bsz) -> sz
            cpf.seek(SeekFrom::Start(offset + bsz as u64))?;

            let len = sz - (offset + bsz as u64);
            let mut verify_vec: Vec<u8> = vec![0; len as usize];
            cpf.read_exact(&mut verify_vec[..])?;

            for i in 0..len {
                if verify_vec[i as usize] != 0 {
                    bail!(
                        "Not isolated! non-zero byte at {}",
                        (offset + bsz as u64) + i
                    );
                }
            }

            // Once done, zero out the write
            cpf.seek(SeekFrom::Start(offset))?;
            cpf.write_all(&vec![0; bsz])?;
        }
    }

    println!("Done ok, waiting on show_work");

    loop {
        let cpf = &mut cpfs[4];
        let wc = cpf.show_work()?;
        println!("Up:{} ds:{}", wc.up_count, wc.ds_count);
        if wc.up_count + wc.ds_count == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    Ok(())
}

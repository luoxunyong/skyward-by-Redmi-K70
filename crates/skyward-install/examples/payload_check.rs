//! Check a package's payload.bin against its own manifest without writing
//! anything: where every operation's data lies, that it hashes to what the
//! manifest says, and that its stretches lie inside the image. Prints the
//! payload's offset in the zip and its data offset, which is what an install
//! counts from.

use anyhow::{bail, Context, Result};
use skyward_install::package::{self, Package};
use skyward_install::payload;
use std::env;
use std::fs::File;

fn main() -> Result<()> {
    let path = env::args().nth(1).context("usage: payload_check <ota.zip|payload.bin>")?;
    let identified = package::identify(File::open(&path).with_context(|| format!("open {path}"))?)?;
    let Package::Update(update) = identified else {
        bail!("{path} is not an update package");
    };
    let payload = payload::open(&path)?;
    println!(
        "payload: {} bytes at {:#x} in the package, data at {:#x} in the payload, {} partitions",
        update.payload_size,
        update.payload_offset,
        payload.data_offset(),
        payload.partitions().len()
    );
    payload::check(&payload, &File::open(&path).with_context(|| format!("open {path}"))?, update.payload_offset)?;
    println!("every operation lies where its manifest says and hashes as it says");
    Ok(())
}

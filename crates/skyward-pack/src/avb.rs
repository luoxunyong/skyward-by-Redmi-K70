//! The footer a recovery image ends in for the bootloader to verify it by:
//! an unsigned vbmeta image holding the image's digest, laid out over the
//! whole partition as avbtool's `add_hash_footer --algorithm NONE` lays it.
//! An unlocked bootloader boots such an image whatever its digest; it gives
//! up on the whole slot when the footer at the partition's end points at
//! anything but a vbmeta image, as a stock footer left behind an image
//! written over the start of the partition does.

use anyhow::{ensure, Result};
use sha2::{Digest, Sha256};

/// The unit avbtool lays an image and its vbmeta image out in.
const BLOCK: usize = 4096;
const FOOTER_LEN: usize = 64;
const HEADER_LEN: usize = 256;
const TAG_HASH: u64 = 2;
const RELEASE: &[u8] = b"skyward-pack";
/// The flags field of a vbmeta header: where it lies and what fastboot's
/// `--disable-verity --disable-verification` sets in it.
const FLAGS_OFFSET: usize = 120;
const HASHTREE_DISABLED: u32 = 1;
const VERIFICATION_DISABLED: u32 = 1 << 1;

/// Turn off verified boot's checks in a stock vbmeta image, as fastboot
/// does to the buffer it writes when given `--disable-verity
/// --disable-verification`. The signature then no longer matches, which is
/// why an unlocked bootloader is what takes such an image.
pub fn disable_verification(image: &mut [u8]) -> Result<()> {
    ensure!(image.len() >= HEADER_LEN, "a vbmeta image is at least {HEADER_LEN} bytes");
    ensure!(&image[..4] == b"AVB0", "the image does not start with an AVB vbmeta header");
    let flags = u32::from_be_bytes(image[FLAGS_OFFSET..FLAGS_OFFSET + 4].try_into().expect("four flag bytes"));
    let flags = flags | HASHTREE_DISABLED | VERIFICATION_DISABLED;
    image[FLAGS_OFFSET..FLAGS_OFFSET + 4].copy_from_slice(&flags.to_be_bytes());
    Ok(())
}

/// `image` grown to fill a `partition_size`-byte partition named
/// `partition`: the image, its vbmeta image from the next block on, and the
/// footer locating that in the partition's last bytes, zeros between.
pub fn footed(image: &[u8], partition: &str, partition_size: usize, salt: &[u8]) -> Result<Vec<u8>> {
    let digest = Sha256::new().chain_update(salt).chain_update(image).finalize();
    let vbmeta = vbmeta(&hash_descriptor(image.len() as u64, partition, salt, &digest));
    let offset = image.len().next_multiple_of(BLOCK);
    let end = offset + vbmeta.len().next_multiple_of(BLOCK);
    ensure!(
        end + BLOCK <= partition_size,
        "a {}-byte image and its vbmeta image do not fit a {partition_size}-byte {partition} partition beside its footer",
        image.len()
    );
    let mut out = Vec::with_capacity(partition_size);
    out.extend_from_slice(image);
    out.resize(offset, 0);
    out.extend_from_slice(&vbmeta);
    out.resize(partition_size - FOOTER_LEN, 0);
    out.extend_from_slice(b"AVBf");
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    for n in [image.len(), offset, vbmeta.len()] {
        out.extend_from_slice(&(n as u64).to_be_bytes());
    }
    out.resize(partition_size, 0);
    Ok(out)
}

/// A descriptor saying the first `image_size` bytes of `partition`, salted
/// with `salt`, hash to `digest` under SHA-256.
fn hash_descriptor(image_size: u64, partition: &str, salt: &[u8], digest: &[u8]) -> Vec<u8> {
    let mut algorithm = [0u8; 32];
    algorithm[..6].copy_from_slice(b"sha256");
    let mut d = Vec::new();
    d.extend_from_slice(&TAG_HASH.to_be_bytes());
    // How many bytes follow these first sixteen, once they are known.
    d.extend_from_slice(&0u64.to_be_bytes());
    d.extend_from_slice(&image_size.to_be_bytes());
    d.extend_from_slice(&algorithm);
    // The lengths of the name, salt and digest, then the flags.
    for n in [partition.len(), salt.len(), digest.len(), 0] {
        d.extend_from_slice(&(n as u32).to_be_bytes());
    }
    d.extend_from_slice(&[0; 60]);
    d.extend_from_slice(partition.as_bytes());
    d.extend_from_slice(salt);
    d.extend_from_slice(digest);
    d.resize(d.len().next_multiple_of(8), 0);
    let following = (d.len() - 16) as u64;
    d[8..16].copy_from_slice(&following.to_be_bytes());
    d
}

/// An unsigned vbmeta image holding `descriptors`: its header, an empty
/// authentication block and the descriptors as its auxiliary block.
fn vbmeta(descriptors: &[u8]) -> Vec<u8> {
    let aux = descriptors.len().next_multiple_of(64);
    let mut v = Vec::with_capacity(HEADER_LEN + aux);
    v.extend_from_slice(b"AVB0");
    // The libavb version it needs, 1.0.
    v.extend_from_slice(&1u32.to_be_bytes());
    v.extend_from_slice(&0u32.to_be_bytes());
    v.extend_from_slice(&0u64.to_be_bytes());
    v.extend_from_slice(&(aux as u64).to_be_bytes());
    // No signing algorithm.
    v.extend_from_slice(&0u32.to_be_bytes());
    // The offsets and sizes of the hash, the signature, the public key, its
    // metadata and the descriptors, then the rollback index.
    let n = descriptors.len();
    for field in [0, 0, 0, 0, n, 0, n, 0, 0, n, 0] {
        v.extend_from_slice(&(field as u64).to_be_bytes());
    }
    // The flags and the rollback index location.
    v.extend_from_slice(&0u32.to_be_bytes());
    v.extend_from_slice(&0u32.to_be_bytes());
    v.extend_from_slice(RELEASE);
    v.resize(HEADER_LEN, 0);
    v.extend_from_slice(descriptors);
    v.resize(HEADER_LEN + aux, 0);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const SALT: [u8; 32] = [0x5a; 32];

    /// What avbtool 1.4.0 makes of `len` bytes of `S`, given by
    /// `add_hash_footer --partition_size 81920 --partition_name recovery
    /// --algorithm NONE --salt 5a…5a --internal_release_string skyward-pack`.
    fn avbtool(len: usize) -> &'static [u8] {
        match len {
            4096 => include_bytes!("../tests/avb-footer-4096.bin"),
            1000 => include_bytes!("../tests/avb-footer-1000.bin"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn disabling_verification_sets_the_two_flags_fastboot_sets() {
        let mut image = vec![0u8; HEADER_LEN];
        image[..4].copy_from_slice(b"AVB0");
        disable_verification(&mut image).unwrap();
        assert_eq!(u32::from_be_bytes(image[FLAGS_OFFSET..FLAGS_OFFSET + 4].try_into().unwrap()), 3);

        let mut short = b"AVB0".to_vec();
        assert!(disable_verification(&mut short).is_err(), "a short image");
        let mut wrong = vec![0u8; HEADER_LEN];
        wrong[..4].copy_from_slice(b"NOPE");
        assert!(disable_verification(&mut wrong).is_err(), "not a vbmeta image");
    }

    #[test]
    fn the_footer_is_avbtools_byte_for_byte() {
        for len in [4096, 1000] {
            let got = footed(&vec![b'S'; len], "recovery", 81920, &SALT).unwrap();
            assert!(got == avbtool(len), "a {len}-byte image");
        }
    }

    #[test]
    fn an_image_that_leaves_no_room_for_its_vbmeta_and_footer_is_refused() {
        let image = vec![b'S'; 8192];
        assert!(footed(&image, "recovery", 16384, &SALT).is_ok(), "a block each for the vbmeta image and the footer");
        assert!(footed(&image, "recovery", 12288, &SALT).is_err());
    }
}

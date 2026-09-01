//! Debian binary packages as a tool archive format.
//!
//! Linux system libraries have no upstream tarball a build can pin, but every
//! distribution publishes immutable, content-addressable `.deb` files of them
//! — `snapshot.debian.org` keeps every version ever released. A `.deb` is an
//! `ar` archive of three members, of which only `data.tar.*` holds the files,
//! so unwrapping one is a few dozen lines and lets a pinned tool name a
//! Debian package directly.

use anyhow::{bail, Context, Result};

const MAGIC: &[u8] = b"!<arch>\n";
const MEMBER_HEADER_LEN: usize = 60;

pub fn is_deb(archive: &[u8]) -> bool {
    archive.starts_with(MAGIC)
}

/// The bytes of the package's `data.tar.*` member, still compressed. Callers
/// hand them to tar, which detects the compression itself.
pub fn data_member(archive: &[u8]) -> Result<&[u8]> {
    if !is_deb(archive) {
        bail!("not an ar archive");
    }
    let mut offset = MAGIC.len();
    while offset + MEMBER_HEADER_LEN <= archive.len() {
        let header = &archive[offset..offset + MEMBER_HEADER_LEN];
        let name = std::str::from_utf8(&header[..16])
            .context("ar member name is not UTF-8")?
            .trim_end();
        let size: usize = std::str::from_utf8(&header[48..58])
            .context("ar member size is not UTF-8")?
            .trim()
            .parse()
            .context("ar member size is not a number")?;
        let body = offset + MEMBER_HEADER_LEN;
        let end = body.checked_add(size).context("ar member size overflows")?;
        if end > archive.len() {
            bail!("ar member `{name}` extends past the end of the archive");
        }
        if name
            .strip_suffix('/')
            .unwrap_or(name)
            .starts_with("data.tar")
        {
            return Ok(&archive[body..end]);
        }
        // Members are padded to an even offset.
        offset = end + end % 2;
    }
    bail!("no data.tar member in the Debian package")
}

#[cfg(test)]
mod tests {
    use super::{data_member, is_deb};

    #[test]
    fn the_data_member_is_found_after_odd_length_members() {
        let archive = archive(&[
            ("debian-binary", b"2.0\n".to_vec()),
            ("control.tar.xz", b"control".to_vec()),
            ("data.tar.xz", b"payload".to_vec()),
        ]);

        assert!(is_deb(&archive));
        assert_eq!(data_member(&archive).unwrap(), b"payload");
    }

    #[test]
    fn a_package_without_a_data_member_is_rejected() {
        let archive = archive(&[("debian-binary", b"2.0\n".to_vec())]);

        assert_eq!(
            data_member(&archive).unwrap_err().to_string(),
            "no data.tar member in the Debian package"
        );
    }

    #[test]
    fn a_truncated_member_is_rejected() {
        let mut archive = archive(&[("data.tar.zst", b"payload".to_vec())]);
        archive.truncate(archive.len() - 3);

        assert!(data_member(&archive)
            .unwrap_err()
            .to_string()
            .contains("extends past the end"));
    }

    fn archive(members: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut bytes = b"!<arch>\n".to_vec();
        for (name, body) in members {
            bytes.extend_from_slice(format!("{name:<16}").as_bytes());
            bytes.extend_from_slice(b"0           0     0     100644  ");
            bytes.extend_from_slice(format!("{:<10}", body.len()).as_bytes());
            bytes.extend_from_slice(b"`\n");
            bytes.extend_from_slice(body);
            if body.len() % 2 == 1 {
                bytes.push(b'\n');
            }
        }
        bytes
    }
}

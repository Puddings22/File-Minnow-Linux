//! Mounted filesystems, from `/proc/self/mountinfo`.
//!
//! Used to recognise network folders (inotify only reports changes made
//! through this computer, so they need scheduled rescans) and removable
//! drives (mounted under the user's media directories).
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub point: PathBuf,
    pub fstype: String,
}

/// Decodes the `\040`-style escapes of mountinfo fields.
fn unescape(field: &str) -> Vec<u8> {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 4 <= bytes.len()
            && let Ok(code) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 4]).unwrap_or("x"), 8)
        {
            out.push(code);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

pub fn parse(mountinfo: &str) -> Vec<Mount> {
    use std::os::unix::ffi::OsStringExt;
    mountinfo
        .lines()
        .filter_map(|line| {
            // mount ID, parent ID, major:minor, root, mount point, options, [optional]... - fstype source superoptions
            let (left, right) = line.split_once(" - ")?;
            let point = left.split(' ').nth(4)?;
            let fstype = right.split(' ').next()?;
            Some(Mount {
                point: PathBuf::from(std::ffi::OsString::from_vec(unescape(point))),
                fstype: fstype.to_owned(),
            })
        })
        .collect()
}

pub fn current() -> Vec<Mount> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|text| parse(&text))
        .unwrap_or_default()
}

/// The mount that holds `path` (the longest mount point that is a prefix).
pub fn mount_of<'a>(mounts: &'a [Mount], path: &Path) -> Option<&'a Mount> {
    mounts
        .iter()
        .filter(|m| path.starts_with(&m.point))
        .max_by_key(|m| m.point.as_os_str().len())
}

/// Whether a filesystem type is served over the network.
pub fn is_network(fstype: &str) -> bool {
    let base = fstype.split('.').next().unwrap_or(fstype);
    matches!(
        base,
        "nfs"
            | "nfs4"
            | "cifs"
            | "smb3"
            | "smbfs"
            | "afs"
            | "ceph"
            | "glusterfs"
            | "9p"
            | "davfs"
            | "ncpfs"
            | "coda"
            | "lustre"
            | "ocfs2"
            | "gfs2"
    ) || matches!(
        fstype,
        "fuse.sshfs"
            | "fuse.rclone"
            | "fuse.gvfsd-fuse"
            | "fuse.davfs2"
            | "fuse.s3fs"
            | "fuse.curlftpfs"
            | "fuse.smbnetfs"
            | "fuse.cifs"
    )
}

/// The filesystem type of `path` when it is on a network mount.
pub fn network_fs(path: &Path) -> Option<String> {
    let mounts = current();
    mount_of(&mounts, path)
        .filter(|m| is_network(&m.fstype))
        .map(|m| m.fstype.clone())
}

/// Where desktops mount removable drives for this user.
pub fn media_dirs() -> Vec<PathBuf> {
    let user = std::env::var("USER")
        .ok()
        .or_else(|| std::env::var("LOGNAME").ok())
        .unwrap_or_default();
    let mut dirs = vec![PathBuf::from("/media"), PathBuf::from("/run/media")];
    if !user.is_empty() {
        dirs.insert(0, PathBuf::from("/media").join(&user));
        dirs.insert(1, PathBuf::from("/run/media").join(&user));
    }
    dirs
}

/// Mount points of removable drives: mounts directly under a media
/// directory (`/media/<user>/USB`, `/run/media/<user>/USB`, `/media/USB`).
pub fn removable(mounts: &[Mount]) -> Vec<PathBuf> {
    let media = media_dirs();
    mounts
        .iter()
        .filter(|m| {
            media
                .iter()
                .any(|dir| m.point.parent() == Some(dir.as_path()))
                && !is_network(&m.fstype)
        })
        .map(|m| m.point.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mountinfo_and_classifies() {
        let text = "\
22 1 0:21 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw
48 22 0:44 / /home/alex/nas rw,relatime shared:2 - cifs //nas/share rw,vers=3.0
49 22 0:45 / /media/alex/USB\\040Stick rw shared:3 - vfat /dev/sdb1 rw
50 22 0:46 / /mnt/remote rw shared:4 - fuse.sshfs alex@host:/ rw
51 22 0:47 / /media/alex/net rw shared:5 - nfs4 host:/export rw
";
        let mounts = parse(text);
        assert_eq!(mounts.len(), 5);
        assert_eq!(mounts[2].point, Path::new("/media/alex/USB Stick"));
        let fs = |p: &str| mount_of(&mounts, Path::new(p)).map(|m| m.fstype.as_str());
        assert_eq!(fs("/home/alex/nas/docs"), Some("cifs"));
        assert_eq!(fs("/home/alex/nasty"), Some("ext4"));
        assert_eq!(fs("/mnt/remote/x"), Some("fuse.sshfs"));
        assert!(is_network("cifs") && is_network("fuse.sshfs") && is_network("nfs4"));
        assert!(!is_network("ext4") && !is_network("vfat") && !is_network("fuse.portal"));
        // SAFETY: the only test that sets USER in this crate.
        unsafe { std::env::set_var("USER", "alex") };
        assert_eq!(removable(&mounts), [PathBuf::from("/media/alex/USB Stick")]);
    }
}

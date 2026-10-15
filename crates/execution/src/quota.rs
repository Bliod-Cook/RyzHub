//! Disk quotas for sandboxes.
//!
//! Writable layers live on a node-level XFS volume mounted with project quotas. Each sandbox's
//! upper and work directories, and each Chat workspace, are one project with a byte limit, as
//! overlay2 with XFS project quotas does: a full sandbox gets ENOSPC while the node keeps
//! running. New files inherit their directory's project. The volume is a loop-mounted image
//! the host creates on first start, so it needs no particular host filesystem.
use std::{
    ffi::CString,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use anyhow::{Context, ensure};

use crate::host::{mountinfo, run};

/// The quota volume: where it is mounted and its block device.
#[derive(Debug)]
pub struct Volume {
    pub mountpoint: PathBuf,
    device: CString,
}

/// `struct fsxattr` from `linux/fs.h`.
#[repr(C)]
#[derive(Default)]
struct FsXattr {
    xflags: u32,
    extsize: u32,
    nextents: u32,
    projid: u32,
    cowextsize: u32,
    pad: [u8; 8],
}

/// `struct fs_disk_quota` from `linux/dqblk_xfs.h`.
#[repr(C)]
#[derive(Default)]
struct FsDiskQuota {
    version: i8,
    flags: i8,
    fieldmask: u16,
    id: u32,
    blk_hardlimit: u64,
    blk_softlimit: u64,
    ino_hardlimit: u64,
    ino_softlimit: u64,
    bcount: u64,
    icount: u64,
    itimer: i32,
    btimer: i32,
    iwarns: u16,
    bwarns: u16,
    itimer_hi: i8,
    btimer_hi: i8,
    rtbtimer_hi: i8,
    padding2: i8,
    rtb_hardlimit: u64,
    rtb_softlimit: u64,
    rtbcount: u64,
    rtbtimer: i32,
    rtbwarns: u16,
    padding3: i16,
    padding4: [u8; 8],
}

const FS_IOC_FSGETXATTR: u32 = 0x801c_581f;
const FS_IOC_FSSETXATTR: u32 = 0x401c_5820;
const FS_XFLAG_PROJINHERIT: u32 = 0x200;
/// `QCMD(Q_XSETQLIM, PRJQUOTA)`.
const Q_XSETQLIM_PROJECT: i32 = (0x5804 << 8) | 2;
const FS_DQUOT_VERSION: i8 = 1;
const FS_PROJ_QUOTA: i8 = 2;
const FS_DQ_BHARD: u16 = 1 << 3;
const FS_DQ_BSOFT: u16 = 1 << 2;

impl Volume {
    /// The volume at `mountpoint`, backed by `image`: created (`size` bytes) and formatted on
    /// first use, attached to a loop device and mounted with project quotas unless it already
    /// is. The image is preallocated unless `sparse`: a sparse image fails writes (and XFS may
    /// shut down) when the disk under it fills.
    pub async fn prepare(
        image: &Path,
        size: u64,
        sparse: bool,
        mountpoint: &Path,
    ) -> anyhow::Result<Self> {
        let image_text = image.to_str().context("INVALID_PATH")?;
        if !image.exists() {
            let partial = image.with_extension("new");
            let file = std::fs::File::create(&partial)?;
            if sparse {
                file.set_len(size)?;
            } else {
                // SAFETY: a valid descriptor and a length in range.
                let result = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, size as i64) };
                ensure!(result == 0, "allocating the quota volume failed: {result}");
            }
            drop(file);
            run(
                "mkfs.xfs",
                &["-q", "-m", "reflink=1", partial.to_str().unwrap()],
            )
            .await?;
            std::fs::rename(&partial, image)?;
        }
        std::fs::create_dir_all(mountpoint)?;
        if mountinfo(mountpoint)?.is_none() {
            let device = match attached(image_text).await? {
                Some(device) => device,
                None => attach(image_text).await?,
            };
            run(
                "mount",
                &[
                    "-t",
                    "xfs",
                    "-o",
                    "prjquota",
                    &device,
                    mountpoint.to_str().context("INVALID_PATH")?,
                ],
            )
            .await?;
        }
        let mount = mountinfo(mountpoint)?.context("QUOTA_VOLUME_NOT_MOUNTED")?;
        ensure!(
            mount
                .options
                .split(',')
                .any(|o| matches!(o, "prjquota" | "pquota")),
            "QUOTA_VOLUME_WITHOUT_PROJECT_QUOTA"
        );
        Ok(Self {
            mountpoint: mountpoint.into(),
            device: CString::new(mount.source)?,
        })
    }

    /// Make `dirs` one project, `project`, limited to `bytes`. Files later created inside
    /// them belong to it.
    pub fn assign(&self, dirs: &[&Path], project: u32, bytes: u64) -> anyhow::Result<()> {
        ensure!(project > 0, "INVALID_QUOTA_PROJECT");
        for dir in dirs {
            ensure!(dir.starts_with(&self.mountpoint), "OUTSIDE_QUOTA_VOLUME");
            set_project(dir, project)?;
        }
        let blocks = bytes.div_ceil(512);
        let mut quota = FsDiskQuota {
            version: FS_DQUOT_VERSION,
            flags: FS_PROJ_QUOTA,
            fieldmask: FS_DQ_BHARD | FS_DQ_BSOFT,
            id: project,
            blk_hardlimit: blocks,
            blk_softlimit: blocks,
            ..Default::default()
        };
        // SAFETY: the device is a NUL-terminated path and `quota` lives through the call.
        let result = unsafe {
            libc::syscall(
                libc::SYS_quotactl,
                Q_XSETQLIM_PROJECT,
                self.device.as_ptr(),
                project as libc::c_int,
                &mut quota as *mut FsDiskQuota,
            )
        };
        ensure!(
            result == 0,
            "setting quota failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }
}

fn set_project(dir: &Path, project: u32) -> anyhow::Result<()> {
    let file = std::fs::File::open(dir)?;
    let mut attributes = FsXattr::default();
    // SAFETY: `attributes` matches the kernel's `struct fsxattr` and outlives both calls.
    let got = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            FS_IOC_FSGETXATTR as _,
            &mut attributes as *mut FsXattr,
        )
    };
    ensure!(
        got == 0,
        "reading project of {} failed: {}",
        dir.display(),
        std::io::Error::last_os_error()
    );
    attributes.projid = project;
    attributes.xflags |= FS_XFLAG_PROJINHERIT;
    let set = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            FS_IOC_FSSETXATTR as _,
            &attributes as *const FsXattr,
        )
    };
    ensure!(
        set == 0,
        "setting project of {} failed: {}",
        dir.display(),
        std::io::Error::last_os_error()
    );
    Ok(())
}

/// The loop device `image` is attached to, if any. `losetup -a` lists `DEVICE: …` lines
/// naming the backing file (busybox: `/dev/loop0: 0 /path`; util-linux:
/// `/dev/loop0: [2049]:12 (/path)`); busybox has no `-j`.
async fn attached(image: &str) -> anyhow::Result<Option<String>> {
    let listing = run("losetup", &["-a"]).await?;
    Ok(listing.lines().find_map(|line| {
        let (device, rest) = line.split_once(':')?;
        rest.split_whitespace()
            .any(|word| word.trim_matches(['(', ')']) == image)
            .then(|| device.to_owned())
    }))
}

/// Attach `image` to a free loop device, creating the device node if this mount namespace
/// lacks it (containers usually only have `/dev/loop-control`).
async fn attach(image: &str) -> anyhow::Result<String> {
    let device = run("losetup", &["-f"]).await?.trim().to_owned();
    let minor: u64 = device
        .strip_prefix("/dev/loop")
        .context("UNEXPECTED_LOOP_DEVICE")?
        .parse()?;
    if !Path::new(&device).exists() {
        nix::sys::stat::mknod(
            device.as_str(),
            nix::sys::stat::SFlag::S_IFBLK,
            nix::sys::stat::Mode::from_bits_truncate(0o660),
            nix::sys::stat::makedev(7, minor),
        )?;
    }
    run("losetup", &[&device, image]).await?;
    Ok(device)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_structures_have_their_abi_sizes() {
        assert_eq!(std::mem::size_of::<FsXattr>(), 28);
        assert_eq!(std::mem::size_of::<FsDiskQuota>(), 112);
        // _IOR('X', 31, struct fsxattr) and _IOW('X', 32, struct fsxattr).
        assert_eq!(FS_IOC_FSGETXATTR, (2 << 30) | (28 << 16) | (0x58 << 8) | 31);
        assert_eq!(FS_IOC_FSSETXATTR, (1 << 30) | (28 << 16) | (0x58 << 8) | 32);
    }
}

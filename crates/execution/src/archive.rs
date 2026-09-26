//! OCI-style upper layers: whiteouts and opaque directories are portable metadata.
use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    fs,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink},
    },
    path::{Component, Path},
};
use tar::{Archive, Builder, EntryType, Header};
fn ignored(path: &Path) -> bool {
    path.components().any(|p| {
        matches!(
            p.as_os_str().to_str(),
            Some(".cache" | "auth.json" | ".env" | ".ssh")
        )
    }) || matches!(
        path.components()
            .next()
            .and_then(|c| c.as_os_str().to_str()),
        Some("tmp" | "run" | "proc" | "dev" | "control" | "gateway")
    )
}
pub fn export(root: &Path, destination: &Path) -> anyhow::Result<()> {
    let encoder = zstd::Encoder::new(fs::File::create(destination)?, 3)?;
    let mut tar = Builder::new(encoder);
    fn walk<W: std::io::Write>(
        tar: &mut Builder<W>,
        root: &Path,
        rel: &Path,
    ) -> anyhow::Result<()> {
        let mut entries = fs::read_dir(root.join(rel))?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = rel.join(entry.file_name());
            if ignored(&path) {
                continue;
            }
            let meta = entry.path().symlink_metadata()?;
            let typ = meta.file_type();
            if typ.is_socket() || typ.is_fifo() {
                continue;
            }
            ensure!(
                !entry.file_name().as_bytes().starts_with(b".wh."),
                "RESERVED_WHITEOUT_NAME"
            );
            let mut header = Header::new_gnu();
            header.set_metadata(&meta);
            header.set_uid(meta.uid() as u64);
            header.set_gid(meta.gid() as u64);
            if typ.is_char_device() {
                ensure!(meta.rdev() == 0, "UNSUPPORTED_DEVICE");
                let mut name = b".wh.".to_vec();
                name.extend(entry.file_name().as_bytes());
                header.set_entry_type(EntryType::Regular);
                header.set_size(0);
                header.set_mode(0);
                header.set_cksum();
                tar.append_data(
                    &mut header,
                    rel.join(std::ffi::OsString::from_vec(name)),
                    std::io::empty(),
                )?;
                continue;
            }
            ensure!(
                typ.is_file() || typ.is_dir() || typ.is_symlink(),
                "UNSUPPORTED_ARCHIVE_ENTRY"
            );
            let mut opaque = false;
            let mut attrs = vec![];
            if !typ.is_symlink() {
                for name in xattr::list(entry.path())? {
                    let n = name.to_string_lossy();
                    if n == "trusted.overlay.opaque" || n == "user.overlay.opaque" {
                        opaque = xattr::get(entry.path(), &name)?.is_some_and(|v| v == b"y");
                        continue;
                    }
                    ensure!(
                        !n.ends_with("overlay.redirect") && !n.ends_with("overlay.metacopy"),
                        "UNSAFE_OVERLAY_XATTR"
                    );
                    if n.starts_with("trusted.overlay.") || n.starts_with("user.overlay.") {
                        continue;
                    }
                    if let Some(value) = xattr::get(entry.path(), &name)? {
                        attrs.push((
                            format!("RYZHUB.xattr.{}", STANDARD.encode(name.as_bytes())),
                            STANDARD.encode(value),
                        ));
                    }
                }
            }
            if !attrs.is_empty() {
                tar.append_pax_extensions(attrs.iter().map(|(k, v)| (k.as_str(), v.as_bytes())))?;
            }
            if typ.is_symlink() {
                header.set_size(0);
                header.set_entry_type(EntryType::Symlink);
                tar.append_link(&mut header, &path, fs::read_link(entry.path())?)?;
            } else if typ.is_file() {
                header.set_cksum();
                tar.append_data(&mut header, &path, fs::File::open(entry.path())?)?;
            } else {
                header.set_entry_type(EntryType::Directory);
                header.set_size(0);
                header.set_cksum();
                tar.append_data(&mut header, &path, std::io::empty())?;
                if opaque {
                    let mut h = Header::new_gnu();
                    h.set_size(0);
                    h.set_mode(0);
                    h.set_cksum();
                    tar.append_data(&mut h, path.join(".wh..wh..opq"), std::io::empty())?;
                }
                walk(tar, root, &path)?;
            }
        }
        Ok(())
    }
    walk(&mut tar, root, Path::new(""))?;
    tar.finish()?;
    tar.into_inner()?.finish()?.sync_all()?;
    Ok(())
}
fn safe(path: &Path) -> anyhow::Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "UNSAFE_ARCHIVE_PATH"
    );
    Ok(())
}
pub fn restore(source: &Path, root: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(root)?;
    ensure!(
        fs::read_dir(root)?.next().is_none(),
        "RESTORE_REQUIRES_EMPTY_UPPER"
    );
    let mut archive = Archive::new(zstd::Decoder::new(fs::File::open(source)?)?);
    let mut total = 0u64;
    let mut directories = vec![];
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        safe(&path)?;
        total = total
            .checked_add(entry.size())
            .context("ARCHIVE_TOO_LARGE")?;
        ensure!(total <= 8 * 1024 * 1024 * 1024, "ARCHIVE_TOO_LARGE");
        let mut parent = root.to_path_buf();
        for part in path.parent().unwrap().components() {
            parent.push(part.as_os_str());
            if parent.exists() {
                ensure!(
                    parent.symlink_metadata()?.is_dir(),
                    "ARCHIVE_SYMLINK_PARENT"
                );
            } else {
                fs::create_dir(&parent)?;
            }
        }
        let target = root.join(&path);
        let name = path.file_name().context("INVALID_ENTRY")?.as_bytes();
        if name == b".wh..wh..opq" {
            ensure!(entry.size() == 0, "INVALID_WHITEOUT");
            xattr::set(&parent, "trusted.overlay.opaque", b"y")?;
            continue;
        }
        if let Some(name) = name.strip_prefix(b".wh.") {
            ensure!(
                !name.is_empty() && name != b"." && name != b".." && entry.size() == 0,
                "INVALID_WHITEOUT"
            );
            let dest = parent.join(std::ffi::OsStr::from_bytes(name));
            let status = std::process::Command::new("mknod")
                .arg(&dest)
                .args(["c", "0", "0"])
                .status()?;
            ensure!(status.success(), "WHITEOUT_CREATE_FAILED");
            continue;
        }
        let typ = entry.header().entry_type();
        let mode = entry.header().mode()? & 0o7777;
        let mtime = filetime::FileTime::from_unix_time(i64::try_from(entry.header().mtime()?)?, 0);
        let uid = u32::try_from(entry.header().uid()?)?;
        let gid = u32::try_from(entry.header().gid()?)?;
        ensure!(
            target.symlink_metadata().is_err()
                || (typ.is_dir() && target.symlink_metadata()?.is_dir()),
            "DUPLICATE_ARCHIVE_ENTRY"
        );
        let mut attrs = vec![];
        if let Some(pax) = entry.pax_extensions()? {
            for e in pax {
                let e = e?;
                if let Some(key) = e.key()?.strip_prefix("RYZHUB.xattr.") {
                    let name = STANDARD.decode(key)?;
                    ensure!(
                        name.starts_with(b"user.")
                            || name == b"security.capability"
                            || name == b"system.posix_acl_access"
                            || name == b"system.posix_acl_default",
                        "UNSAFE_XATTR"
                    );
                    attrs.push((
                        std::ffi::OsString::from_vec(name),
                        STANDARD.decode(e.value_bytes())?,
                    ));
                }
            }
        }
        if typ.is_dir() {
            fs::create_dir_all(&target)?;
            directories.push((target.clone(), mode, mtime));
        } else if typ.is_file() {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)?;
            std::io::copy(&mut entry, &mut f)?;
            f.sync_all()?;
            fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
        } else if typ.is_symlink() {
            let link = entry.link_name()?.context("MISSING_LINK")?;
            symlink(link, &target)?;
        } else {
            anyhow::bail!("UNSUPPORTED_ARCHIVE_ENTRY");
        }
        std::os::unix::fs::lchown(&target, Some(uid), Some(gid))?;
        if typ.is_file() {
            fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
        }
        if !typ.is_symlink() {
            for (name, value) in attrs {
                xattr::set(&target, &name, &value)?;
            }
        }
        if typ.is_symlink() {
            filetime::set_symlink_file_times(&target, mtime, mtime)?;
        } else if typ.is_file() {
            filetime::set_file_mtime(&target, mtime)?;
        }
    }
    for (path, mode, mtime) in directories.into_iter().rev() {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
        filetime::set_file_mtime(path, mtime)?;
    }
    fs::File::open(root)?.sync_all()?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_and_symlink_escape() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        fs::create_dir_all(src.join("workspace")).unwrap();
        fs::write(src.join("workspace/untracked"), "hello").unwrap();
        fs::create_dir_all(src.join("workspace/target")).unwrap();
        fs::write(src.join("workspace/target/only-copy"), "preserved").unwrap();
        let mtime = filetime::FileTime::from_unix_time(1234567890, 0);
        filetime::set_file_mtime(src.join("workspace/untracked"), mtime).unwrap();
        fs::set_permissions(
            src.join("workspace/untracked"),
            fs::Permissions::from_mode(0o751),
        )
        .unwrap();
        xattr::set(src.join("workspace/untracked"), "user.test", b"\0bytes").unwrap();
        symlink("/etc", src.join("link")).unwrap();
        let tar = d.path().join("a.tar");
        export(&src, &tar).unwrap();
        let out = d.path().join("out");
        restore(&tar, &out).unwrap();
        assert_eq!(fs::read(out.join("workspace/untracked")).unwrap(), b"hello");
        assert_eq!(
            fs::read(out.join("workspace/target/only-copy")).unwrap(),
            b"preserved"
        );
        assert_eq!(
            fs::metadata(out.join("workspace/untracked"))
                .unwrap()
                .mtime(),
            1234567890
        );
        assert_eq!(
            fs::metadata(out.join("workspace/untracked"))
                .unwrap()
                .mode()
                & 0o777,
            0o751
        );
        assert_eq!(
            xattr::get(out.join("workspace/untracked"), "user.test")
                .unwrap()
                .unwrap(),
            b"\0bytes"
        );
        assert!(safe(Path::new("../../etc")).is_err());
        assert!(safe(Path::new("/etc/passwd")).is_err());
    }
}

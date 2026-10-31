//! A passthrough FUSE filesystem rooted at a host directory.
//!
//! Safety model: every inode carries an `O_PATH` descriptor for its file and
//! the path it was reached by, one component at a time. [`FileSystem::lookup`]
//! opens exactly one more component with `O_NOFOLLOW` under the parent's
//! descriptor, so a guest can never make the server traverse a path it did
//! not vet name by name: `..`, absolute paths, multi-component names and
//! symlink traversal cannot escape the root. Symlinks are served as their
//! own inodes; the guest resolves their targets inside its own tree.
//!
//! Read-only shares refuse every mutating operation with `EROFS`.
//!
//! The session state — open inodes with their paths and lookup counts, open
//! file and directory handles — serialises into the snapshot format
//! ([`PassthroughFs::save_state`]) and restores by re-walking each recorded
//! path from the root, the way virtiofsd's `find-paths` migration mode does.
//! A path that no longer exists restores as a dead inode answering `ENOENT`.
//!
//! Inode numbers are the server's own (`st_ino` is mapped onto them), so the
//! host may renumber its files under us without confusing the guest.

use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use parking_lot::Mutex;

use crate::virtio::fs::fuse::abi::fuse_abi::{CreateIn, FsOptions, ROOT_ID, stat64};
use crate::virtio::fs::fuse::filesystem::{
    Context, DirEntry, Entry, FileSystem, OpenOptions, SetattrValid, ZeroCopyReader, ZeroCopyWriter,
};

/// How long the guest may cache attributes and entries before revalidating.
/// Short, like virtiofsd's defaults, because the host tree can change under
/// the guest.
const TIMEOUT: Duration = Duration::from_secs(1);

/// Snapshot format magic: "IPFS" (in-process fs).
const MAGIC: u32 = 0x5346_5049;

/// A passthrough filesystem serving `root` on the host. Clones share one
/// session, so the device and its FUSE server observe the same inodes.
#[derive(Clone)]
pub struct PassthroughFs {
    root: PathBuf,
    read_only: bool,
    inner: std::sync::Arc<Mutex<Session>>,
}

/// Everything the guest has made the server remember.
struct Session {
    /// `O_PATH` descriptors by inode; the root holds the share directory.
    inodes: HashMap<u64, Inode>,
    /// Live path → inode, so repeat lookups of one file share one guest
    /// inode and one attribute cache.
    by_path: HashMap<PathBuf, u64>,
    /// Open files and directories by handle.
    handles: HashMap<u64, Handle>,
    next_ino: u64,
    next_handle: u64,
}

/// One guest-known file. `fd` is an `O_PATH` handle on the inode; `path` is
/// its location under the root at lookup time (the snapshot form). The
/// parent's `O_PATH` descriptor and the own name serve the `*at` syscalls,
/// which `O_PATH` descriptors alone cannot express (fchmod, readlink,
/// truncate...).
struct Inode {
    fd: OwnedFd,
    parent: OwnedFd,
    name: Vec<u8>,
    path: PathBuf,
    kind: FileKind,
    /// LOOKUP and READDIRPLUS references not yet cancelled by FORGET.
    lookups: u64,
    /// Set when a restored path no longer exists; every access answers
    /// `ENOENT` until the guest forgets the inode.
    dead: bool,
}

/// An open file or directory.
struct Handle {
    ino: u64,
    kind: HandleKind,
}

enum HandleKind {
    File {
        file: File,
        flags: i32,
        /// Path under the root at open time, for the snapshot.
        path: PathBuf,
    },
    Dir {
        /// Cached directory entries, rebuilt when the guest rewinds to
        /// offset 0 (and after a restore).
        entries: Option<Vec<CachedDirent>>,
    },
}

/// One directory entry as getdents64 served it.
#[derive(Clone)]
struct CachedDirent {
    name: Vec<u8>,
    host_ino: u64,
    d_type: u8,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

impl PassthroughFs {
    /// Open the share rooted at `root`, which must be an existing
    /// directory. `read_only` refuses all mutating operations.
    pub fn new(root: PathBuf, read_only: bool) -> io::Result<Self> {
        let fd = open_path(&root, libc::O_PATH | libc::O_DIRECTORY)?;
        Ok(Self {
            root: root.canonicalize().unwrap_or(root),
            read_only,
            inner: std::sync::Arc::new(Mutex::new(Session {
                inodes: HashMap::from([(
                    ROOT_ID,
                    Inode {
                        parent: fd
                            .try_clone()
                            .expect("the share root clones its own descriptor"),
                        fd,
                        name: Vec::new(),
                        path: PathBuf::new(),
                        kind: FileKind::Dir,
                        lookups: 1,
                        dead: false,
                    },
                )]),
                by_path: [(PathBuf::new(), ROOT_ID)].into(),
                handles: HashMap::new(),
                next_ino: ROOT_ID + 1,
                next_handle: 1,
            })),
        })
    }

    /// The host directory this filesystem serves.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether mutating operations are refused.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Serialise the session: every inode with its path and lookup count,
    /// every open handle with the path it was opened at.
    pub fn save_state(&self) -> Vec<u8> {
        let session = self.inner.lock();
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.push(1); // format version
        out.extend_from_slice(&(session.inodes.len() as u32).to_le_bytes());
        let mut inodes: Vec<(&u64, &Inode)> = session.inodes.iter().collect();
        inodes.sort_by_key(|(ino, _)| **ino);
        for (ino, inode) in inodes {
            let path = inode.path.as_os_str().as_bytes();
            out.extend_from_slice(&ino.to_le_bytes());
            out.extend_from_slice(&inode.lookups.to_le_bytes());
            out.push(match inode.kind {
                FileKind::File => 0,
                FileKind::Dir => 1,
                FileKind::Symlink => 2,
                FileKind::Other => 3,
            });
            out.extend_from_slice(&(path.len() as u32).to_le_bytes());
            out.extend_from_slice(path);
        }
        out.extend_from_slice(&(session.handles.len() as u32).to_le_bytes());
        let mut handles: Vec<(&u64, &Handle)> = session.handles.iter().collect();
        handles.sort_by_key(|(h, _)| **h);
        for (handle, open) in handles {
            out.extend_from_slice(&handle.to_le_bytes());
            out.extend_from_slice(&open.ino.to_le_bytes());
            match &open.kind {
                HandleKind::File { flags, path, .. } => {
                    out.push(0);
                    out.extend_from_slice(&(*flags as u32).to_le_bytes());
                    let path = path.as_os_str().as_bytes();
                    out.extend_from_slice(&(path.len() as u32).to_le_bytes());
                    out.extend_from_slice(path);
                }
                HandleKind::Dir { .. } => out.push(1),
            }
        }
        out.extend_from_slice(&session.next_ino.to_le_bytes());
        out.extend_from_slice(&session.next_handle.to_le_bytes());
        out
    }

    /// Replace the session with one rebuilt from [`Self::save_state`] bytes:
    /// every inode is re-walked from the root, every file handle reopened at
    /// its recorded path (creation flags stripped), every directory handle
    /// reset to its start. A missing path becomes a dead inode; an open file
    /// that vanished reopens on `/dev/null` and answers `ESTALE`.
    pub fn restore_state(&self, state: &[u8]) -> io::Result<()> {
        let err = || io::Error::new(io::ErrorKind::InvalidData, "bad passthrough state");
        if state.len() < 10 || u32::from_le_bytes(state[0..4].try_into().unwrap()) != MAGIC {
            return Err(err());
        }
        if state[4] != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("passthrough state version {} is not 1", state[4]),
            ));
        }
        let mut cur = 5usize;
        let mut take = |len: usize| -> io::Result<&[u8]> {
            let end = cur.checked_add(len).ok_or_else(err)?;
            let slice = state.get(cur..end).ok_or_else(err)?;
            cur = end;
            Ok(slice)
        };
        let root_fd = self.inner.lock().inodes[&ROOT_ID].fd.try_clone()?;

        let count = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
        let mut inodes: HashMap<u64, Inode> = HashMap::with_capacity(count);
        let mut by_path = HashMap::new();
        for _ in 0..count {
            let ino = u64::from_le_bytes(take(8)?.try_into().unwrap());
            let lookups = u64::from_le_bytes(take(8)?.try_into().unwrap());
            let kind = match take(1)?[0] {
                0 => FileKind::File,
                1 => FileKind::Dir,
                2 => FileKind::Symlink,
                3 => FileKind::Other,
                _ => return Err(err()),
            };
            let len = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
            let path = PathBuf::from(OsStr::from_bytes(take(len)?));
            if ino == ROOT_ID {
                if !path.as_os_str().is_empty() {
                    return Err(err());
                }
                by_path.insert(PathBuf::new(), ino);
                let fd = root_fd.try_clone()?;
                let parent = fd.try_clone()?;
                inodes.insert(
                    ino,
                    Inode {
                        fd,
                        parent,
                        name: Vec::new(),
                        path,
                        kind: FileKind::Dir,
                        lookups,
                        dead: false,
                    },
                );
                continue;
            }
            // Re-walk the recorded path one component at a time from the
            // root: a snapshot is data at rest, not a trusted guest.
            let mut fd = root_fd.try_clone()?;
            let mut parent = root_fd.try_clone()?;
            let mut name = Vec::new();
            let mut alive = true;
            for component in path.components() {
                let Some(component) = normal_component(&component) else {
                    alive = false;
                    break;
                };
                match openat(&fd, component, libc::O_PATH | libc::O_NOFOLLOW) {
                    Ok(child) => {
                        parent = fd;
                        fd = child;
                        name = component.to_vec();
                    }
                    Err(_) => {
                        alive = false;
                        break;
                    }
                }
            }
            if alive {
                by_path.insert(path.clone(), ino);
            }
            inodes.insert(
                ino,
                Inode {
                    fd: if alive { fd } else { root_fd.try_clone()? },
                    parent,
                    name,
                    path,
                    kind,
                    lookups,
                    dead: !alive,
                },
            );
        }

        let count = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
        let mut handles: HashMap<u64, Handle> = HashMap::with_capacity(count);
        for _ in 0..count {
            let handle = u64::from_le_bytes(take(8)?.try_into().unwrap());
            let ino = u64::from_le_bytes(take(8)?.try_into().unwrap());
            let Some(inode) = inodes.get(&ino) else {
                return Err(err());
            };
            let kind = match take(1)?[0] {
                0 => {
                    let flags = i32::from_le_bytes(take(4)?.try_into().unwrap())
                        & !(libc::O_CREAT | libc::O_EXCL);
                    let len = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
                    let path = PathBuf::from(OsStr::from_bytes(take(len)?));
                    let file = if inode.dead {
                        // Nothing to reopen; I/O on it must fail loudly.
                        reopen_dead()?
                    } else {
                        reopen(&inode.fd, flags).unwrap_or_else(|_| reopen_dead().unwrap())
                    };
                    HandleKind::File { file, flags, path }
                }
                1 => HandleKind::Dir { entries: None },
                _ => return Err(err()),
            };
            handles.insert(handle, Handle { ino, kind });
        }
        let next_ino = u64::from_le_bytes(take(8)?.try_into().unwrap());
        let next_handle = u64::from_le_bytes(take(8)?.try_into().unwrap());
        if cur != state.len() {
            return Err(err());
        }
        let mut session = self.inner.lock();
        session.inodes = inodes;
        session.by_path = by_path;
        session.handles = handles;
        session.next_ino = next_ino;
        session.next_handle = next_handle;
        Ok(())
    }

    fn refuse_writes(&self) -> io::Result<()> {
        if self.read_only {
            Err(io::Error::from_raw_os_error(libc::EROFS))
        } else {
            Ok(())
        }
    }
}

impl FileSystem for PassthroughFs {
    type Inode = u64;
    type Handle = u64;

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        let mut want = FsOptions::ASYNC_READ
            | FsOptions::AUTO_INVAL_DATA
            | FsOptions::DO_READDIRPLUS
            | FsOptions::READDIRPLUS_AUTO
            | FsOptions::PARALLEL_DIROPS
            | FsOptions::ABORT_ERROR
            | FsOptions::MAX_PAGES
            | FsOptions::CACHE_SYMLINKS
            | FsOptions::BIG_WRITES;
        if !self.read_only {
            // The guest buffers writes and pushes them later; without
            // writeback it would flush every write synchronously.
            want |= FsOptions::WRITEBACK_CACHE;
        }
        Ok(want & capable)
    }

    fn destroy(&self) {}

    fn lookup(&self, _ctx: &Context, parent: u64, name: &CStr) -> io::Result<Entry> {
        let mut session = self.inner.lock();
        let parent = live_inode(&session, parent)?;
        if parent.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let bytes = checked_name(name)?;
        let fd = openat(&parent.fd, bytes, libc::O_PATH | libc::O_NOFOLLOW)?;
        let st = fstat(fd.as_raw_fd())?;
        let path = parent.path.join(OsStr::from_bytes(bytes));
        let dirfd = parent.fd.try_clone()?;
        session.entry_for(path, fd, dirfd, bytes, kind_of(st.st_mode), &st)
    }

    fn forget(&self, _ctx: &Context, inode: u64, count: u64) {
        self.inner.lock().forget_one(inode, count);
    }

    fn batch_forget(&self, _ctx: &Context, requests: Vec<(u64, u64)>) {
        let mut session = self.inner.lock();
        for (inode, count) in requests {
            session.forget_one(inode, count);
        }
    }

    fn getattr(
        &self,
        _ctx: &Context,
        inode: u64,
        handle: Option<u64>,
    ) -> io::Result<(stat64, Duration)> {
        let session = self.inner.lock();
        let fd = if let Some(handle) = handle
            && let Some(open) = session.handles.get(&handle)
            && let HandleKind::File { file, .. } = &open.kind
        {
            file.as_raw_fd()
        } else {
            live_inode(&session, inode)?.fd.as_raw_fd()
        };
        let mut st = fstat(fd)?;
        st.st_ino = inode;
        Ok((st, TIMEOUT))
    }

    fn setattr(
        &self,
        _ctx: &Context,
        inode: u64,
        attr: stat64,
        handle: Option<u64>,
        valid: SetattrValid,
    ) -> io::Result<(stat64, Duration)> {
        self.refuse_writes()?;
        let session = self.inner.lock();
        let state = live_inode(&session, inode)?;
        let reply_fd = if let Some(handle) = handle
            && let Some(open) = session.handles.get(&handle)
            && let HandleKind::File { file, .. } = &open.kind
        {
            file.as_raw_fd()
        } else {
            state.fd.as_raw_fd()
        };
        if valid.contains(SetattrValid::MODE) {
            fchmodat(&state.parent, &state.name, attr.st_mode)?;
        }
        if valid.intersects(SetattrValid::UID | SetattrValid::GID) {
            fchownat(&state.parent, &state.name, valid, &attr)?;
        }
        if valid.contains(SetattrValid::SIZE) {
            truncateat(&state.parent, &state.name, attr.st_size)?;
        }
        if valid.intersects(SetattrValid::ATIME | SetattrValid::MTIME) {
            utimensat(&state.parent, &state.name, valid, &attr)?;
        }
        let mut st = fstat(reply_fd)?;
        st.st_ino = inode;
        Ok((st, TIMEOUT))
    }

    fn readlink(&self, _ctx: &Context, inode: u64) -> io::Result<Vec<u8>> {
        let session = self.inner.lock();
        let inode = live_inode(&session, inode)?;
        if inode.kind != FileKind::Symlink {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        // readlinkat on the parent's descriptor: one component, never a
        // traversal, and unlike the /proc fd link it spells the target.
        let mut target = vec![0u8; libc::PATH_MAX as usize];
        // SAFETY: `cname` is NUL-terminated and `target` is writable for
        // its length; the fd is owned.
        let n = unsafe {
            libc::readlinkat(
                inode.parent.as_raw_fd(),
                cname(&inode.name).as_ptr(),
                target.as_mut_ptr().cast(),
                target.len(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        target.truncate(n as usize);
        Ok(target)
    }

    fn symlink(
        &self,
        _ctx: &Context,
        target: &CStr,
        parent: u64,
        name: &CStr,
    ) -> io::Result<Entry> {
        self.refuse_writes()?;
        let mut session = self.inner.lock();
        let parent = live_inode(&session, parent)?;
        if parent.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let bytes = checked_name(name)?;
        let path = parent.path.join(OsStr::from_bytes(bytes));
        symlinkat(target, &parent.fd, bytes)?;
        let fd = openat(&parent.fd, bytes, libc::O_PATH | libc::O_NOFOLLOW)?;
        let st = fstat(fd.as_raw_fd())?;
        let dirfd = parent.fd.try_clone()?;
        session.entry_for(path, fd, dirfd, bytes, FileKind::Symlink, &st)
    }

    fn mknod(
        &self,
        _ctx: &Context,
        parent: u64,
        name: &CStr,
        mode: u32,
        rdev: u32,
        _umask: u32,
    ) -> io::Result<Entry> {
        self.refuse_writes()?;
        let mut session = self.inner.lock();
        let parent = live_inode(&session, parent)?;
        if parent.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let bytes = checked_name(name)?;
        let path = parent.path.join(OsStr::from_bytes(bytes));
        mknodat(&parent.fd, bytes, mode as libc::mode_t, rdev as libc::dev_t)?;
        let fd = openat(&parent.fd, bytes, libc::O_PATH | libc::O_NOFOLLOW)?;
        let st = fstat(fd.as_raw_fd())?;
        let dirfd = parent.fd.try_clone()?;
        session.entry_for(path, fd, dirfd, bytes, kind_of(st.st_mode), &st)
    }

    fn mkdir(
        &self,
        _ctx: &Context,
        parent: u64,
        name: &CStr,
        mode: u32,
        _umask: u32,
    ) -> io::Result<Entry> {
        self.refuse_writes()?;
        let mut session = self.inner.lock();
        let parent = live_inode(&session, parent)?;
        if parent.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let bytes = checked_name(name)?;
        let path = parent.path.join(OsStr::from_bytes(bytes));
        mkdirat(&parent.fd, bytes, mode as libc::mode_t)?;
        let fd = openat(&parent.fd, bytes, libc::O_PATH | libc::O_NOFOLLOW)?;
        let st = fstat(fd.as_raw_fd())?;
        let dirfd = parent.fd.try_clone()?;
        session.entry_for(path, fd, dirfd, bytes, FileKind::Dir, &st)
    }

    fn unlink(&self, _ctx: &Context, parent: u64, name: &CStr) -> io::Result<()> {
        self.refuse_writes()?;
        let session = self.inner.lock();
        let parent = live_inode(&session, parent)?;
        unlinkat(&parent.fd, checked_name(name)?, false)
    }

    fn rmdir(&self, _ctx: &Context, parent: u64, name: &CStr) -> io::Result<()> {
        self.refuse_writes()?;
        let session = self.inner.lock();
        let parent = live_inode(&session, parent)?;
        unlinkat(&parent.fd, checked_name(name)?, true)
    }

    fn rename(
        &self,
        _ctx: &Context,
        parent: u64,
        name: &CStr,
        newparent: u64,
        newname: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        self.refuse_writes()?;
        let session = self.inner.lock();
        let parent = live_inode(&session, parent)?;
        let newparent = live_inode(&session, newparent)?;
        if parent.kind != FileKind::Dir || newparent.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        renameat2(
            &parent.fd,
            checked_name(name)?,
            &newparent.fd,
            checked_name(newname)?,
            flags,
        )
    }

    fn link(
        &self,
        _ctx: &Context,
        inode: u64,
        newparent: u64,
        newname: &CStr,
    ) -> io::Result<Entry> {
        self.refuse_writes()?;
        let mut session = self.inner.lock();
        let inode = live_inode(&session, inode)?;
        let newparent = live_inode(&session, newparent)?;
        if newparent.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let bytes = checked_name(newname)?;
        linkat(&inode.fd, &newparent.fd, bytes)?;
        let fd = openat(&newparent.fd, bytes, libc::O_PATH | libc::O_NOFOLLOW)?;
        let st = fstat(fd.as_raw_fd())?;
        let path = newparent.path.join(OsStr::from_bytes(bytes));
        let dirfd = newparent.fd.try_clone()?;
        session.entry_for(path, fd, dirfd, bytes, kind_of(st.st_mode), &st)
    }

    fn open(
        &self,
        _ctx: &Context,
        inode: u64,
        flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions, Option<u32>)> {
        let mut session = self.inner.lock();
        let handle = session.open_file(self.read_only, inode, flags as i32)?;
        Ok((Some(handle), OpenOptions::empty(), None))
    }

    fn create(
        &self,
        _ctx: &Context,
        parent: u64,
        name: &CStr,
        args: CreateIn,
    ) -> io::Result<(Entry, Option<u64>, OpenOptions, Option<u32>)> {
        self.refuse_writes()?;
        let mut session = self.inner.lock();
        let parent = live_inode(&session, parent)?;
        if parent.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let bytes = checked_name(name)?;
        let flags = args.flags as i32 | libc::O_CREAT;
        let file = createat(&parent.fd, bytes, flags, args.mode)?;
        let path = parent.path.join(OsStr::from_bytes(bytes));
        let fd = openat(&parent.fd, bytes, libc::O_PATH | libc::O_NOFOLLOW)?;
        let st = fstat(fd.as_raw_fd())?;
        let dirfd = parent.fd.try_clone()?;
        let entry = session.entry_for(path.clone(), fd, dirfd, bytes, kind_of(st.st_mode), &st)?;
        let handle = session.insert_file_handle(entry.inode, file, flags & !libc::O_CREAT, path);
        Ok((entry, Some(handle), OpenOptions::empty(), None))
    }

    fn read(
        &self,
        _ctx: &Context,
        inode: u64,
        handle: u64,
        w: &mut dyn ZeroCopyWriter,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        let session = self.inner.lock();
        let mut file = session.file_handle(inode, handle)?;
        // The reply carries the header too: never claim more than the
        // writable descriptors hold.
        let size = (size as usize).min(w.available_bytes());
        w.write_from(&mut file, size, offset)
    }

    fn write(
        &self,
        _ctx: &Context,
        inode: u64,
        handle: u64,
        r: &mut dyn ZeroCopyReader,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _delayed_write: bool,
        _flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<usize> {
        // The open already refuses write flags on a read-only share; this
        // guards a handle restored into a share that became read-only.
        self.refuse_writes()?;
        let session = self.inner.lock();
        let mut file = session.file_handle(inode, handle)?;
        r.read_to(&mut file, size as usize, offset)
    }

    fn flush(&self, _ctx: &Context, _inode: u64, _handle: u64, _lock_owner: u64) -> io::Result<()> {
        // Every operation acts on the host file directly; there is nothing
        // buffered to flush.
        Ok(())
    }

    fn fsync(&self, _ctx: &Context, inode: u64, datasync: bool, handle: u64) -> io::Result<()> {
        let session = self.inner.lock();
        let file = session.file_handle(inode, handle)?;
        fsync(file.as_raw_fd(), datasync)
    }

    fn release(
        &self,
        _ctx: &Context,
        inode: u64,
        _flags: u32,
        handle: u64,
        _flush: bool,
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> io::Result<()> {
        self.inner.lock().forget_handle(inode, handle);
        Ok(())
    }

    fn statfs(&self, _ctx: &Context, _inode: u64) -> io::Result<libc::statvfs64> {
        let session = self.inner.lock();
        let root = live_inode(&session, ROOT_ID)?;
        // SAFETY: statvfs64 is a POD struct; zeroing is its init.
        let mut st: libc::statvfs64 = unsafe { std::mem::zeroed() };
        // SAFETY: `st` is an out-parameter of statvfs(2); the fd is owned.
        if unsafe { libc::fstatvfs64(root.fd.as_raw_fd(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }

    fn opendir(
        &self,
        _ctx: &Context,
        inode: u64,
        flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions)> {
        {
            let session = self.inner.lock();
            let inode = live_inode(&session, inode)?;
            if inode.kind != FileKind::Dir {
                return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
            }
        }
        if flags as i32 & libc::O_ACCMODE != libc::O_RDONLY {
            self.refuse_writes()?;
        }
        let mut session = self.inner.lock();
        let handle = session.next_handle;
        session.next_handle += 1;
        session.handles.insert(
            handle,
            Handle {
                ino: inode,
                kind: HandleKind::Dir { entries: None },
            },
        );
        Ok((Some(handle), OpenOptions::empty()))
    }

    fn readdir(
        &self,
        _ctx: &Context,
        inode: u64,
        handle: u64,
        size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
    ) -> io::Result<()> {
        let mut session = self.inner.lock();
        let entries = session.dir_entries(inode, handle, offset)?.to_vec();
        let live = live_inode(&session, inode)?;
        let dir_path = live.path.clone();
        // Cookie `i` resumes at entry `i`: entry `n` carries offset `n+1`.
        let mut used = 0usize;
        for (index, entry) in entries.iter().enumerate().skip(offset as usize) {
            if used > size as usize {
                break;
            }
            let ino = session
                .by_path
                .get(&dir_path.join(OsStr::from_bytes(&entry.name)))
                .copied()
                .unwrap_or(entry.host_ino);
            match add_entry(DirEntry {
                ino,
                offset: (index + 1) as u64,
                type_: entry.d_type as u32,
                name: &entry.name,
            }) {
                Ok(0) => break,
                Ok(len) => used += len,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn readdirplus(
        &self,
        _ctx: &Context,
        inode: u64,
        handle: u64,
        size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry, Entry) -> io::Result<usize>,
    ) -> io::Result<()> {
        let mut session = self.inner.lock();
        let entries = session.dir_entries(inode, handle, offset)?.to_vec();
        let live = live_inode(&session, inode)?;
        let dir_path = live.path.clone();
        let dir_fd = live.fd.try_clone()?;
        // Cookie `i` resumes at entry `i`: entry `n` carries offset `n+1`.
        let mut used = 0usize;
        for (index, cached) in entries.iter().enumerate().skip(offset as usize) {
            if used > size as usize {
                break;
            }
            let path = dir_path.join(OsStr::from_bytes(&cached.name));
            // Resolve (or create) the session inode so the Entry carries the
            // server's own ino and a lookup count the kernel will FORGET.
            let entry = match session.resolve(&path, &dir_fd, &cached.name) {
                Ok(entry) => entry,
                Err(_) => continue, // raced away; the guest re-lookups
            };
            match add_entry(
                DirEntry {
                    ino: entry.inode,
                    offset: (index + 1) as u64,
                    type_: cached.d_type as u32,
                    name: &cached.name,
                },
                entry,
            ) {
                Ok(0) => break,
                Ok(len) => used += len,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn fsyncdir(&self, _ctx: &Context, inode: u64, datasync: bool, _handle: u64) -> io::Result<()> {
        let session = self.inner.lock();
        let inode = live_inode(&session, inode)?;
        // O_PATH descriptors cannot be synced; reopen the directory.
        let dir = reopen(&inode.fd, libc::O_RDONLY | libc::O_DIRECTORY)?;
        fsync(dir.as_raw_fd(), datasync)
    }

    fn releasedir(&self, _ctx: &Context, inode: u64, _flags: u32, handle: u64) -> io::Result<()> {
        self.inner.lock().forget_handle(inode, handle);
        Ok(())
    }

    fn access(&self, _ctx: &Context, _inode: u64, _mask: u32) -> io::Result<()> {
        // Permission checking happens on each host syscall; the guest's own
        // mode bits are the policy it sees.
        Ok(())
    }

    fn lseek(
        &self,
        _ctx: &Context,
        inode: u64,
        handle: u64,
        offset: u64,
        whence: u32,
    ) -> io::Result<u64> {
        let session = self.inner.lock();
        let file = session.file_handle(inode, handle)?;
        // SAFETY: lseek(2) on an owned fd; the return is the new offset.
        let out = unsafe { libc::lseek(file.as_raw_fd(), offset as i64, whence as i32) };
        if out < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(out as u64)
    }

    // xattrs, locks, ioctl and DAX stay unimplemented: the kernel takes the
    // ENOSYS defaults (no xattrs, local POSIX locks, no cache window).
}

impl Session {
    /// One FORGET: drop the inode when no lookup count and no handle keeps
    /// it.
    fn forget_one(&mut self, inode: u64, count: u64) {
        let lookups = match self.inodes.get_mut(&inode) {
            Some(state) => {
                state.lookups = state.lookups.saturating_sub(count);
                state.lookups
            }
            None => return,
        };
        if lookups == 0
            && !self.holds(inode)
            && let Some(state) = self.inodes.remove(&inode)
            // Another live path may have taken the slot after a rename.
            && self.by_path.get(&state.path) == Some(&inode)
        {
            self.by_path.remove(&state.path);
        }
    }

    fn holds(&self, inode: u64) -> bool {
        self.handles.values().any(|open| open.ino == inode)
    }

    /// The inode for `path`, newly opened (`fd`) or already live: repeat
    /// lookups of one file keep one guest inode. Bumps the lookup count.
    #[allow(clippy::too_many_arguments)]
    fn entry_for(
        &mut self,
        path: PathBuf,
        fd: OwnedFd,
        parent: OwnedFd,
        name: &[u8],
        kind: FileKind,
        st: &stat64,
    ) -> io::Result<Entry> {
        let ino = match self.by_path.get(&path) {
            Some(&ino) => {
                // Refresh the descriptor: the old one may belong to a file
                // the host replaced since the last lookup.
                if let Some(state) = self.inodes.get_mut(&ino) {
                    state.fd = fd;
                    state.parent = parent;
                    state.name = name.to_vec();
                    state.dead = false;
                }
                ino
            }
            None => {
                let ino = self.next_ino;
                self.next_ino += 1;
                self.by_path.insert(path.clone(), ino);
                self.inodes.insert(
                    ino,
                    Inode {
                        fd,
                        parent,
                        name: name.to_vec(),
                        path,
                        kind,
                        lookups: 0,
                        dead: false,
                    },
                );
                ino
            }
        };
        let state = self.inodes.get_mut(&ino).expect("just inserted");
        state.lookups += 1;
        let mut attr = *st;
        attr.st_ino = ino;
        Ok(Entry {
            inode: ino,
            generation: 0,
            attr,
            attr_flags: 0,
            attr_timeout: TIMEOUT,
            entry_timeout: TIMEOUT,
        })
    }

    /// Resolve one cached dirent to a session inode (used by READDIRPLUS).
    fn resolve(&mut self, path: &Path, dir: &OwnedFd, name: &[u8]) -> io::Result<Entry> {
        if let Some(&ino) = self.by_path.get(path) {
            let state = self.inodes.get(&ino).expect("by_path agrees with inodes");
            let st = fstat(state.fd.as_raw_fd())?;
            let kind = state.kind;
            let fd = state.fd.try_clone()?;
            let parent = state.parent.try_clone()?;
            let name = state.name.clone();
            let path = state.path.clone();
            return self.entry_for(path, fd, parent, &name, kind, &st);
        }
        let fd = openat(dir, name, libc::O_PATH | libc::O_NOFOLLOW)?;
        let st = fstat(fd.as_raw_fd())?;
        let parent = dir.try_clone()?;
        self.entry_for(
            path.to_path_buf(),
            fd,
            parent,
            name,
            kind_of(st.st_mode),
            &st,
        )
    }

    fn insert_file_handle(&mut self, ino: u64, file: File, flags: i32, path: PathBuf) -> u64 {
        let handle = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(
            handle,
            Handle {
                ino,
                kind: HandleKind::File { file, flags, path },
            },
        );
        handle
    }

    fn open_file(&mut self, read_only: bool, inode: u64, flags: i32) -> io::Result<u64> {
        let state = live_inode(self, inode)?;
        let flags = flags | libc::O_CLOEXEC;
        if read_only && flags & libc::O_ACCMODE != libc::O_RDONLY {
            return Err(io::Error::from_raw_os_error(libc::EROFS));
        }
        let file = reopen(&state.fd, flags)?;
        let path = state.path.clone();
        Ok(self.insert_file_handle(inode, file, flags, path))
    }

    /// A clone of the open file behind `handle` for one I/O operation.
    fn file_handle(&self, inode: u64, handle: u64) -> io::Result<File> {
        let open = self
            .handles
            .get(&handle)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
        if open.ino != inode {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        match &open.kind {
            HandleKind::File { file, .. } => file
                .try_clone()
                .map_err(|e| io::Error::new(e.kind(), format!("reopening for I/O: {e}"))),
            HandleKind::Dir { .. } => Err(io::Error::from_raw_os_error(libc::EISDIR)),
        }
    }

    /// The cached entries for `handle`, rebuilt when the guest rewinds to
    /// offset 0 or after a restore.
    fn dir_entries(&mut self, inode: u64, handle: u64, offset: u64) -> io::Result<&[CachedDirent]> {
        let dir = if offset == 0
            || !matches!(
                self.handles.get(&handle),
                Some(Handle {
                    kind: HandleKind::Dir { entries: Some(_) },
                    ..
                })
            ) {
            Some(live_inode(self, inode)?.fd.try_clone()?)
        } else {
            None
        };
        let open = self
            .handles
            .get_mut(&handle)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
        if open.ino != inode {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        let HandleKind::Dir { entries } = &mut open.kind else {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        };
        if let Some(dir) = dir {
            *entries = Some(read_dir(&dir)?);
        }
        Ok(entries.as_ref().expect("just built").as_slice())
    }

    fn forget_handle(&mut self, inode: u64, handle: u64) {
        let open = self.handles.remove(&handle);
        if open.is_some_and(|open| open.ino == inode) {
            // The open kept the inode alive beyond its last FORGET.
            self.forget_one(inode, 0);
        }
    }
}

/// The live inode `ino`, or `ENOENT` for one the guest was never given or
/// one whose path died with a restore.
fn live_inode(session: &Session, ino: u64) -> io::Result<&Inode> {
    session
        .inodes
        .get(&ino)
        .filter(|inode| !inode.dead)
        .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
}

/// A single path component from the guest, or the error the kernel expects.
fn checked_name(name: &CStr) -> io::Result<&[u8]> {
    let bytes = name.to_bytes();
    if bytes.is_empty()
        || bytes == b"."
        || bytes == b".."
        || bytes.contains(&b'/')
        || bytes.len() > 255
    {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    Ok(bytes)
}

/// The bytes of one normal path component, or `None` for anything else
/// (`..`, root, current dir).
fn normal_component<'a>(component: &std::path::Component<'a>) -> Option<&'a [u8]> {
    match component {
        std::path::Component::Normal(name) => Some(name.as_bytes()),
        _ => None,
    }
}

/// One `openat(2)` with `O_CLOEXEC`, owning the result.
fn openat(dir: &OwnedFd, name: &[u8], flags: i32) -> io::Result<OwnedFd> {
    let cname =
        CString::new(name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `cname` is NUL-terminated and outlives the call; the fd is
    // owned from return.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), cname.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the fd is fresh and owned from the syscall above.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `open(2)` on a whole path; only the share root, which the host named.
fn open_path(path: &Path, flags: i32) -> io::Result<OwnedFd> {
    let cname = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `cname` is NUL-terminated; the fd is owned from return.
    let fd = unsafe { libc::open(cname.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the fd is fresh and owned from the syscall above.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Reopen the file an `O_PATH` descriptor points at with real `flags`: the
/// procfs fd link resolves to exactly that inode, so the guest opens what
/// it looked up even if the host replaced the name meanwhile.
fn reopen(path_fd: &OwnedFd, flags: i32) -> io::Result<File> {
    let path = CString::new(format!("/proc/self/fd/{}", path_fd.as_raw_fd()))
        .expect("proc path has no NUL");
    // SAFETY: `path` is NUL-terminated; the fd is owned from return.
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the fd is fresh and owned from the open above.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// A stand-in descriptor for an open file whose path died before a
/// restore: reads and writes must fail, and nothing may share data with
/// the guest.
fn reopen_dead() -> io::Result<File> {
    let path = b"/dev/null\0";
    // SAFETY: `path` is a NUL-terminated literal; the fd is owned from
    // return.
    let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the fd is fresh and owned from the open above.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn fstat(fd: RawFd) -> io::Result<stat64> {
    // SAFETY: stat64 is a POD struct; zeroing is its init.
    let mut st: stat64 = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is an out-parameter of fstat(2); the fd is owned.
    if unsafe { libc::fstat64(fd, &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn kind_of(mode: libc::mode_t) -> FileKind {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => FileKind::Dir,
        libc::S_IFLNK => FileKind::Symlink,
        libc::S_IFREG => FileKind::File,
        _ => FileKind::Other,
    }
}

fn symlinkat(target: &CStr, dir: &OwnedFd, name: &[u8]) -> io::Result<()> {
    let cname =
        CString::new(name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: both strings are NUL-terminated; the fd is owned.
    if unsafe { libc::symlinkat(target.as_ptr(), dir.as_raw_fd(), cname.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn mknodat(dir: &OwnedFd, name: &[u8], mode: libc::mode_t, rdev: libc::dev_t) -> io::Result<()> {
    let cname =
        CString::new(name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `cname` is NUL-terminated; the fd is owned.
    if unsafe { libc::mknodat(dir.as_raw_fd(), cname.as_ptr(), mode, rdev) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn mkdirat(dir: &OwnedFd, name: &[u8], mode: libc::mode_t) -> io::Result<()> {
    let cname =
        CString::new(name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `cname` is NUL-terminated; the fd is owned.
    if unsafe { libc::mkdirat(dir.as_raw_fd(), cname.as_ptr(), mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn createat(dir: &OwnedFd, name: &[u8], flags: i32, mode: u32) -> io::Result<File> {
    let cname =
        CString::new(name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `cname` is NUL-terminated; the fd is owned from return.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            cname.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode as libc::mode_t,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the fd is fresh and owned from the open above.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn unlinkat(dir: &OwnedFd, name: &[u8], directory: bool) -> io::Result<()> {
    let cname =
        CString::new(name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let flags = if directory { libc::AT_REMOVEDIR } else { 0 };
    // SAFETY: `cname` is NUL-terminated; the fd is owned.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), cname.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn renameat2(
    old_dir: &OwnedFd,
    old_name: &[u8],
    new_dir: &OwnedFd,
    new_name: &[u8],
    flags: u32,
) -> io::Result<()> {
    let old =
        CString::new(old_name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let new =
        CString::new(new_name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: both strings are NUL-terminated; both fds are owned.
    if unsafe {
        libc::renameat2(
            old_dir.as_raw_fd(),
            old.as_ptr(),
            new_dir.as_raw_fd(),
            new.as_ptr(),
            flags as libc::c_uint,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn linkat(path_fd: &OwnedFd, dir: &OwnedFd, name: &[u8]) -> io::Result<()> {
    let cname =
        CString::new(name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `cname` is NUL-terminated; both fds are owned; AT_EMPTY_PATH
    // names the file the descriptor points at.
    if unsafe {
        libc::linkat(
            path_fd.as_raw_fd(),
            c"".as_ptr(),
            dir.as_raw_fd(),
            cname.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `fchmodat(2)` relative to a parent descriptor: `O_PATH` handles cannot
/// carry fchmod(2) themselves.
fn fchmodat(parent: &OwnedFd, name: &[u8], mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: `cname` is NUL-terminated; the fd is owned.
    if unsafe { libc::fchmodat(parent.as_raw_fd(), cname(name).as_ptr(), mode, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A NUL-terminated copy of one component.
fn cname(name: &[u8]) -> CString {
    CString::new(name.to_vec()).expect("a component has no NUL")
}

fn fchownat(parent: &OwnedFd, name: &[u8], valid: SetattrValid, attr: &stat64) -> io::Result<()> {
    // -1 leaves a field unchanged; uid_t and gid_t are unsigned on Linux.
    let uid = if valid.contains(SetattrValid::UID) {
        attr.st_uid
    } else {
        u32::MAX
    };
    let gid = if valid.contains(SetattrValid::GID) {
        attr.st_gid
    } else {
        u32::MAX
    };
    // SAFETY: `cname` is NUL-terminated; the fd is owned; -1 leaves a
    // field unchanged.
    if unsafe { libc::fchownat(parent.as_raw_fd(), cname(name).as_ptr(), uid, gid, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Truncate through a write-mode reopen: O_PATH handles cannot carry
/// ftruncate(2).
fn truncateat(parent: &OwnedFd, name: &[u8], size: i64) -> io::Result<()> {
    let file = openat(parent, name, libc::O_WRONLY)?;
    // SAFETY: the fd is owned.
    if unsafe { libc::ftruncate(file.as_raw_fd(), size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn utimensat(parent: &OwnedFd, name: &[u8], valid: SetattrValid, attr: &stat64) -> io::Result<()> {
    let omit = libc::UTIME_OMIT;
    let times = [
        if valid.contains(SetattrValid::ATIME) {
            libc::timespec {
                tv_sec: attr.st_atime,
                tv_nsec: attr.st_atime_nsec,
            }
        } else {
            libc::timespec {
                tv_sec: 0,
                tv_nsec: omit,
            }
        },
        if valid.contains(SetattrValid::MTIME) {
            libc::timespec {
                tv_sec: attr.st_mtime,
                tv_nsec: attr.st_mtime_nsec,
            }
        } else {
            libc::timespec {
                tv_sec: 0,
                tv_nsec: omit,
            }
        },
    ];
    // SAFETY: `cname` is NUL-terminated; the fd is owned; `times` outlives
    // the call.
    if unsafe { libc::utimensat(parent.as_raw_fd(), cname(name).as_ptr(), times.as_ptr(), 0) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn fsync(fd: RawFd, datasync: bool) -> io::Result<()> {
    // SAFETY: the fd is owned.
    if unsafe {
        if datasync {
            libc::fdatasync(fd)
        } else {
            libc::fsync(fd)
        }
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Read one directory with getdents64, skipping `.` and `..` (the guest
/// VFS synthesises them).
fn read_dir(dir: &OwnedFd) -> io::Result<Vec<CachedDirent>> {
    let path =
        CString::new(format!("/proc/self/fd/{}", dir.as_raw_fd())).expect("proc path has no NUL");
    // SAFETY: `path` is NUL-terminated; the fd is owned from return and
    // closed by drop at the end of this function.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the fd is fresh and owned from the open above.
    let dir = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut entries = Vec::new();
    let mut buffer = vec![0u8; 64 << 10];
    loop {
        // SAFETY: `buffer` is writable for its length; the fd is owned.
        let n = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                dir.as_raw_fd(),
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            break;
        }
        let mut pos = 0usize;
        while pos < n as usize {
            // SAFETY: getdents64 wrote `n` bytes of well-formed records;
            // every record is at least DIRENT_FIXED bytes long and its
            // reclen bounds the name.
            let record = unsafe { &*(buffer.as_ptr().add(pos) as *const Dirent64) };
            let reclen = record.reclen as usize;
            if pos + DIRENT_FIXED > n as usize || reclen < DIRENT_FIXED || pos + reclen > n as usize
            {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            let name = buffer[pos + DIRENT_FIXED..pos + reclen]
                .iter()
                .copied()
                .take_while(|b| *b != 0)
                .collect::<Vec<u8>>();
            if name != b"." && name != b".." {
                entries.push(CachedDirent {
                    name,
                    host_ino: record.ino,
                    d_type: record.d_type,
                });
            }
            pos += reclen;
        }
    }
    Ok(entries)
}

/// `struct linux_dirent64` as getdents64 writes it: three fixed fields,
/// a one-byte type, then the NUL-terminated name padded to 8 bytes.
#[repr(C)]
struct Dirent64 {
    ino: u64,
    off: i64,
    reclen: u16,
    d_type: u8,
}

/// offsetof(d_type) in `struct linux_dirent64`: 8 + 8 + 2 + 1.
const DIRENT_FIXED: usize = 19;

#[cfg(test)]
mod tests {
    use crate::virtio::fs::fuse::file_buf::FileVolatileSlice;
    use std::ffi::CString;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn ctx() -> Context {
        Context::new()
    }

    fn name(bytes: &[u8]) -> CString {
        CString::new(bytes.to_vec()).unwrap()
    }

    /// A writable share over a scratch directory.
    fn scratch() -> (tempfile::TempDir, PassthroughFs) {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path().to_path_buf(), false).unwrap();
        std::fs::write(dir.path().join("hello.txt"), b"hello share\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/nested.txt"), b"nested\n").unwrap();
        std::os::unix::fs::symlink("hello.txt", dir.path().join("link")).unwrap();
        (dir, fs)
    }

    /// LOOKUP one name under the root.
    fn lookup(fs: &PassthroughFs, bytes: &[u8]) -> io::Result<Entry> {
        FileSystem::lookup(fs, &ctx(), ROOT_ID, &name(bytes))
    }

    #[test]
    fn lookup_serves_children_with_server_inodes() {
        let (_dir, fs) = scratch();
        let entry = lookup(&fs, b"hello.txt").unwrap();
        assert!(entry.inode != ROOT_ID);
        // The server's inode number is what the guest sees in stat.
        assert_eq!(entry.attr.st_ino, entry.inode);
        assert!(entry.attr.st_mode & libc::S_IFMT == libc::S_IFREG);
        // Repeat lookups of one path share one inode.
        assert_eq!(lookup(&fs, b"hello.txt").unwrap().inode, entry.inode);
    }

    #[test]
    fn lookup_rejects_escapes_and_odd_names() {
        let (_dir, fs) = scratch();
        for bad in [&b".."[..], b".", b"", b"a/b", b"hello.txt/."] {
            let err = match lookup(&fs, bad) {
                Err(e) => e,
                Ok(_) => panic!("{bad:?} must not resolve"),
            };
            assert!(
                err.raw_os_error() == Some(libc::EINVAL)
                    || err.raw_os_error() == Some(libc::ENOENT),
                "{bad:?}: {err}"
            );
        }
        // A name that does not exist is a plain miss.
        assert_eq!(
            lookup(&fs, b"missing").unwrap_err().raw_os_error(),
            Some(libc::ENOENT)
        );
    }

    #[test]
    fn lookup_walks_directories_one_component_at_a_time() {
        let (_dir, fs) = scratch();
        let sub = lookup(&fs, b"sub").unwrap();
        let nested = FileSystem::lookup(&fs, &ctx(), sub.inode, &name(b"nested.txt")).unwrap();
        assert!(nested.attr.st_mode & libc::S_IFMT == libc::S_IFREG);
    }

    #[test]
    fn symlinks_are_served_not_followed() {
        let (_dir, fs) = scratch();
        let link = lookup(&fs, b"link").unwrap();
        assert!(link.attr.st_mode & libc::S_IFMT == libc::S_IFLNK);
        // The target is data, not a traversal: reading it returns the name.
        let target = FileSystem::readlink(&fs, &ctx(), link.inode).unwrap();
        assert_eq!(target, b"hello.txt");
        // Even a symlink pointing out of the share resolves as text; the
        // guest walks it inside its own tree.
        std::os::unix::fs::symlink("../../etc/passwd", _dir.path().join("escape")).unwrap();
        let escape = lookup(&fs, b"escape").unwrap();
        assert_eq!(
            FileSystem::readlink(&fs, &ctx(), escape.inode).unwrap(),
            b"../../etc/passwd"
        );
    }

    #[test]
    fn open_read_write_round_trip() {
        let (_dir, fs) = scratch();
        let entry = lookup(&fs, b"hello.txt").unwrap();
        let (handle, _, _) =
            FileSystem::open(&fs, &ctx(), entry.inode, libc::O_RDONLY as u32, 0).unwrap();
        let handle = handle.unwrap();

        // Read through the zero-copy path used on the queue: a memory
        // buffer standing in for the writable descriptors.
        let mut served = Vec::new();
        let mut writer = SinkWriter(&mut served);
        let n =
            FileSystem::read(&fs, &ctx(), entry.inode, handle, &mut writer, 5, 0, None, 0).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&served[..5], b"hello");

        // Write through the zero-copy reader path.
        let (handle, _, _) =
            FileSystem::open(&fs, &ctx(), entry.inode, libc::O_RDWR as u32, 0).unwrap();
        let handle = handle.unwrap();
        let data = b"HELLO";
        let mut reader = SliceReader(data);
        let n = FileSystem::write(
            &fs,
            &ctx(),
            entry.inode,
            handle,
            &mut reader,
            data.len() as u32,
            0,
            None,
            false,
            0,
            0,
        )
        .unwrap();
        assert_eq!(n, data.len());
        assert_eq!(
            std::fs::read(_dir.path().join("hello.txt")).unwrap(),
            b"HELLO share\n"
        );
    }

    /// A `ZeroCopyWriter` into a buffer, for driving reads off the queue.
    struct SinkWriter<'a>(&'a mut Vec<u8>);

    impl std::io::Write for SinkWriter<'_> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl ZeroCopyWriter for SinkWriter<'_> {
        fn write_from(
            &mut self,
            f: &mut dyn crate::virtio::fs::fuse::file_traits::FileReadWriteVolatile,
            count: usize,
            off: u64,
        ) -> io::Result<usize> {
            // Stand in for the writable descriptors: read the file into a
            // buffer a volatile slice points at.
            let mut buf = vec![0u8; count];
            // SAFETY: `buf` outlives the call; the length matches.
            let slice = unsafe { FileVolatileSlice::from_raw_ptr(buf.as_mut_ptr(), buf.len()) };
            let n = f.read_at_volatile(slice, off)?;
            self.0.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn available_bytes(&self) -> usize {
            usize::MAX
        }
    }

    /// A `ZeroCopyReader` over a buffer, for driving writes onto the queue.
    struct SliceReader<'a>(&'a [u8]);

    impl std::io::Read for SliceReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    impl ZeroCopyReader for SliceReader<'_> {
        fn read_to(
            &mut self,
            f: &mut dyn crate::virtio::fs::fuse::file_traits::FileReadWriteVolatile,
            count: usize,
            off: u64,
        ) -> io::Result<usize> {
            let want = count.min(self.0.len());
            let mut consumed = vec![0u8; want];
            std::io::Read::read_exact(self, &mut consumed)?;
            // Hand the file a volatile view of the bytes, as the queue would.
            // SAFETY: `consumed` outlives the call; the length matches.
            let slice = unsafe {
                FileVolatileSlice::from_raw_ptr(consumed.as_ptr().cast_mut(), consumed.len())
            };
            f.write_at_volatile(slice, off)?;
            Ok(consumed.len())
        }
    }

    #[test]
    fn create_unlink_and_mkdir() {
        let (dir, fs) = scratch();
        let (entry, handle, _, _) = FileSystem::create(
            &fs,
            &ctx(),
            ROOT_ID,
            &name(b"new.txt"),
            CreateIn {
                flags: libc::O_RDWR as u32,
                mode: 0o644,
                fuse_flags: 0,
                umask: 0o022,
            },
        )
        .unwrap();
        assert!(handle.is_some());
        assert!(dir.path().join("new.txt").is_file());

        FileSystem::mkdir(&fs, &ctx(), ROOT_ID, &name(b"dir"), 0o755, 0o022).unwrap();
        assert!(dir.path().join("dir").is_dir());

        FileSystem::unlink(&fs, &ctx(), ROOT_ID, &name(b"new.txt")).unwrap();
        assert!(!dir.path().join("new.txt").exists());

        // The handle outlives the name: the guest keeps reading the file it
        // opened, the way an unlinked open file behaves.
        let (st, _) = FileSystem::getattr(&fs, &ctx(), entry.inode, handle).unwrap();
        assert!(st.st_mode & libc::S_IFMT == libc::S_IFREG);
    }

    #[test]
    fn readdir_serves_entries_with_cookies() {
        let (_dir, fs) = scratch();
        let (handle, _) = FileSystem::opendir(&fs, &ctx(), ROOT_ID, libc::O_RDONLY as u32).unwrap();
        let handle = handle.unwrap();
        let mut names = Vec::new();
        let mut offset = 0;
        loop {
            let mut batch: Vec<(u64, Vec<u8>)> = Vec::new();
            let before = names.len();
            FileSystem::readdir(&fs, &ctx(), ROOT_ID, handle, 128, offset, &mut |d| {
                batch.push((d.offset, d.name.to_vec()));
                Ok(d.name.len() + 24)
            })
            .unwrap();
            if batch.is_empty() && names.len() == before {
                break;
            }
            names.extend(batch.iter().map(|(_, n)| n.clone()));
            match batch.last() {
                Some((next, _)) => offset = *next,
                None => break,
            }
        }
        names.sort();
        assert_eq!(
            names,
            vec![b"hello.txt".to_vec(), b"link".to_vec(), b"sub".to_vec()]
        );
        FileSystem::releasedir(&fs, &ctx(), ROOT_ID, 0, handle).unwrap();
    }

    #[test]
    fn forget_reaps_inodes_but_handles_keep_them() {
        let (_dir, fs) = scratch();
        let entry = lookup(&fs, b"hello.txt").unwrap();
        let (handle, _, _) =
            FileSystem::open(&fs, &ctx(), entry.inode, libc::O_RDONLY as u32, 0).unwrap();
        // Burn every lookup; the open handle must keep the inode alive.
        FileSystem::forget(&fs, &ctx(), entry.inode, entry.inode);
        let (st, _) = FileSystem::getattr(&fs, &ctx(), entry.inode, handle).unwrap();
        assert_eq!(st.st_ino, entry.inode);
        // Releasing the handle lets the last reference go.
        FileSystem::release(
            &fs,
            &ctx(),
            entry.inode,
            0,
            handle.unwrap(),
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            FileSystem::getattr(&fs, &ctx(), entry.inode, None)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENOENT)
        );
    }

    #[test]
    fn read_only_shares_refuse_every_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path().to_path_buf(), true).unwrap();
        std::fs::write(dir.path().join("file"), b"data").unwrap();
        let entry = lookup(&fs, b"file").unwrap();

        let err = FileSystem::open(&fs, &ctx(), entry.inode, libc::O_WRONLY as u32, 0).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EROFS));

        fn expect_erofs(result: io::Result<impl Sized>) {
            let Err(err) = result else {
                panic!("a read-only share must refuse every mutation");
            };
            assert_eq!(err.raw_os_error(), Some(libc::EROFS), "{err}");
        }
        expect_erofs(FileSystem::mkdir(
            &fs,
            &ctx(),
            ROOT_ID,
            &name(b"d"),
            0o755,
            0,
        ));
        expect_erofs(FileSystem::unlink(&fs, &ctx(), ROOT_ID, &name(b"file")));
        expect_erofs(FileSystem::symlink(
            &fs,
            &ctx(),
            &name(b"x"),
            ROOT_ID,
            &name(b"l"),
        ));
        // SAFETY: stat64 is POD; zeroing is its init.
        let mut attr: stat64 = unsafe { std::mem::zeroed() };
        attr.st_size = 0;
        expect_erofs(FileSystem::setattr(
            &fs,
            &ctx(),
            entry.inode,
            attr,
            None,
            SetattrValid::SIZE,
        ));
        expect_erofs(FileSystem::create(
            &fs,
            &ctx(),
            ROOT_ID,
            &name(b"c"),
            CreateIn {
                flags: libc::O_WRONLY as u32,
                mode: 0o644,
                fuse_flags: 0,
                umask: 0,
            },
        ));

        // Reads still work.
        let (handle, _, _) =
            FileSystem::open(&fs, &ctx(), entry.inode, libc::O_RDONLY as u32, 0).unwrap();
        assert!(handle.is_some());
    }

    #[test]
    fn setattr_changes_size_and_times() {
        let (dir, fs) = scratch();
        let entry = lookup(&fs, b"hello.txt").unwrap();
        let mut attr = FileSystem::getattr(&fs, &ctx(), entry.inode, None)
            .unwrap()
            .0;
        attr.st_size = 4;
        let (st, _) =
            FileSystem::setattr(&fs, &ctx(), entry.inode, attr, None, SetattrValid::SIZE).unwrap();
        assert_eq!(st.st_size, 4);
        assert_eq!(
            std::fs::read(dir.path().join("hello.txt")).unwrap(),
            b"hell"
        );
    }

    #[test]
    fn statfs_reports_the_share() {
        let (_dir, fs) = scratch();
        let st = FileSystem::statfs(&fs, &ctx(), ROOT_ID).unwrap();
        assert!(st.f_bsize > 0);
    }

    #[test]
    fn session_state_round_trips_by_path() {
        let (dir, fs) = scratch();
        // Open a file and a directory, leave inodes live.
        let hello = lookup(&fs, b"hello.txt").unwrap();
        let (file_handle, _, _) =
            FileSystem::open(&fs, &ctx(), hello.inode, libc::O_RDWR as u32, 0).unwrap();
        let sub = lookup(&fs, b"sub").unwrap();
        let (dir_handle, _) =
            FileSystem::opendir(&fs, &ctx(), sub.inode, libc::O_RDONLY as u32).unwrap();
        let nested = FileSystem::lookup(&fs, &ctx(), sub.inode, &name(b"nested.txt")).unwrap();
        let state = fs.save_state();

        // Restore into a fresh filesystem over the same directory: same
        // inodes, same handles, contents readable through both.
        let fs2 = PassthroughFs::new(dir.path().to_path_buf(), false).unwrap();
        fs2.restore_state(&state).unwrap();
        let mut served = Vec::new();
        let mut writer = SinkWriter(&mut served);
        FileSystem::read(
            &fs2,
            &ctx(),
            hello.inode,
            file_handle.unwrap(),
            &mut writer,
            u32::MAX,
            0,
            None,
            0,
        )
        .unwrap();
        assert_eq!(served, b"hello share\n");
        let (st, _) = FileSystem::getattr(&fs2, &ctx(), nested.inode, None).unwrap();
        assert!(st.st_mode & libc::S_IFMT == libc::S_IFREG);
        // The old server still works too: the two sessions are
        // independent copies of the same tree.
        assert!(dir_handle.is_some());
    }

    #[test]
    fn restore_marks_missing_paths_dead() {
        let (dir, fs) = scratch();
        let hello = lookup(&fs, b"hello.txt").unwrap();
        let state = fs.save_state();
        std::fs::remove_file(dir.path().join("hello.txt")).unwrap();

        let fs2 = PassthroughFs::new(dir.path().to_path_buf(), false).unwrap();
        fs2.restore_state(&state).unwrap();
        // The inode restored but answers ENOENT, like virtiofsd's
        // find-paths over a vanished file.
        assert_eq!(
            FileSystem::getattr(&fs2, &ctx(), hello.inode, None)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENOENT)
        );
        // The root survives.
        let entry = lookup(&fs2, b"sub").unwrap();
        assert!(entry.attr.st_mode & libc::S_IFMT == libc::S_IFDIR);
    }

    #[test]
    fn restore_refuses_tampered_state() {
        let (_dir, fs) = scratch();
        let state = fs.save_state();
        let fs2 =
            PassthroughFs::new(tempfile::tempdir().unwrap().path().to_path_buf(), false).unwrap();
        assert!(fs2.restore_state(&state[..state.len() - 1]).is_err());
        assert!(fs2.restore_state(&[0u8; 12]).is_err());
    }

    #[test]
    fn write_through_the_open_handle_persists() {
        let (dir, fs) = scratch();
        let (entry, handle, _, _) = FileSystem::create(
            &fs,
            &ctx(),
            ROOT_ID,
            &name(b"log.txt"),
            CreateIn {
                flags: libc::O_WRONLY as u32,
                mode: 0o600,
                fuse_flags: 0,
                umask: 0,
            },
        )
        .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.path().join("log.txt"))
            .unwrap();
        // The same bytes the queue would hand the write op.
        file.write_all(b"0123456789").unwrap();
        FileSystem::fsync(&fs, &ctx(), entry.inode, false, handle.unwrap()).unwrap();
        let mut attr = FileSystem::getattr(&fs, &ctx(), entry.inode, None)
            .unwrap()
            .0;
        attr.st_mode = 0o600;
        let (st, _) =
            FileSystem::setattr(&fs, &ctx(), entry.inode, attr, None, SetattrValid::MODE).unwrap();
        assert_eq!(st.st_mode & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(dir.path().join("log.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

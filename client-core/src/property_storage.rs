//! cold property bytes can live in a temp file to save some ram.
//! normally they stay in memory. ROVE_PROPERTY_CACHE_MIB=1..16 turns on a
//! bounded read cache with a private temp file, unlinked right away on unix.
//! the bytes stay lossless. no credentials or network traffic go through here.
use std::{borrow::Cow, sync::Arc};

pub fn statistics() -> serde_json::Value {
    #[cfg(any(unix, windows))]
    {
        paged::statistics()
    }
    #[cfg(not(any(unix, windows)))]
    {
        serde_json::json!({"supported":false,"stores":0})
    }
}

#[derive(Clone, Debug)]
pub(crate) enum StoredBytes {
    Memory(Arc<[u8]>),
    #[cfg(any(unix, windows))]
    Paged {
        store: Arc<paged::Store>,
        offset: u32,
        length: u32,
    },
}

impl StoredBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        #[cfg(any(unix, windows))]
        if bytes.len() >= 64 {
            if let Some(store) = paged::current() {
                match store.append(&bytes) {
                    Ok(offset) => {
                        return Self::Paged {
                            store,
                            offset,
                            length: bytes.len() as u32,
                        }
                    }
                    Err(_) => paged::record_memory_fallback(bytes.len()),
                }
            }
        }
        Self::Memory(bytes.into())
    }
    pub fn len(&self) -> usize {
        match self {
            Self::Memory(bytes) => bytes.len(),
            #[cfg(any(unix, windows))]
            Self::Paged { length, .. } => *length as usize,
        }
    }
    pub fn read(&self) -> Cow<'_, [u8]> {
        match self {
            Self::Memory(bytes) => Cow::Borrowed(bytes),
            #[cfg(any(unix, windows))]
            Self::Paged {
                store,
                offset,
                length,
            } => Cow::Owned(
                store
                    .read(*offset, *length)
                    .expect("private property storage read failed"),
            ),
        }
    }
    #[cfg(test)]
    pub fn shares_backing(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Memory(a), Self::Memory(b)) => Arc::ptr_eq(a, b),
            #[cfg(any(unix, windows))]
            (
                Self::Paged {
                    store: a,
                    offset: ao,
                    length: al,
                },
                Self::Paged {
                    store: b,
                    offset: bo,
                    length: bl,
                },
            ) => Arc::ptr_eq(a, b) && ao == bo && al == bl,
            #[allow(unreachable_patterns)]
            _ => false,
        }
    }
}

#[cfg(any(unix, windows))]
mod paged {
    #[cfg(unix)]
    use std::os::{
        fd::FromRawFd,
        unix::{ffi::OsStrExt, fs::FileExt},
    };
    use std::{
        cell::RefCell,
        collections::VecDeque,
        fs::File,
        io,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, Mutex, OnceLock, Weak,
        },
    };

    #[cfg(windows)]
    #[derive(Debug)]
    struct TemporaryDirectory(std::path::PathBuf);
    #[cfg(windows)]
    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    #[cfg(windows)]
    fn write_all_at(mut file: &File, bytes: &[u8], offset: u64) -> io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        // Every operation is inside Store's mutex, including the file cursor.
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(bytes)
    }
    #[cfg(unix)]
    fn write_all_at(file: &File, bytes: &[u8], offset: u64) -> io::Result<()> {
        file.write_all_at(bytes, offset)
    }
    #[cfg(windows)]
    fn read_exact_at(mut file: &File, bytes: &mut [u8], offset: u64) -> io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(bytes)
    }
    #[cfg(unix)]
    fn read_exact_at(file: &File, bytes: &mut [u8], offset: u64) -> io::Result<()> {
        file.read_exact_at(bytes, offset)
    }
    const PAGE: usize = 64 * 1024;
    const MAX_FILE: usize = 100 * 1024 * 1024;
    thread_local! { static CURRENT: RefCell<Weak<Store>> = const { RefCell::new(Weak::new()) }; }
    static STORES: OnceLock<Mutex<Vec<Weak<Store>>>> = OnceLock::new();
    static MEMORY_FALLBACKS: AtomicU64 = AtomicU64::new(0);
    static MEMORY_FALLBACK_BYTES: AtomicU64 = AtomicU64::new(0);
    static CREATE_FAILURES: AtomicU64 = AtomicU64::new(0);

    pub(super) fn record_memory_fallback(bytes: usize) {
        MEMORY_FALLBACKS.fetch_add(1, Ordering::Relaxed);
        MEMORY_FALLBACK_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(super) fn statistics() -> serde_json::Value {
        let mut stores = 0usize;
        let mut backing = 0u64;
        let mut cached = 0usize;
        let mut active = 0usize;
        if let Some(registry) = STORES.get() {
            let mut registry = registry.lock().unwrap_or_else(|e| e.into_inner());
            registry.retain(|entry| entry.strong_count() > 0);
            for store in registry.iter().filter_map(Weak::upgrade) {
                let inner = store.inner.lock().unwrap_or_else(|e| e.into_inner());
                stores += 1;
                backing += inner.committed;
                active += inner.active.capacity();
                cached += inner
                    .cache
                    .iter()
                    .map(|(_, page)| page.len())
                    .sum::<usize>();
            }
        }
        serde_json::json!({"supported":true,"stores":stores,"disk_backing_bytes":backing,
            "read_cache_bytes":cached,"write_buffer_bytes":active,
            "memory_fallback_events":MEMORY_FALLBACKS.load(Ordering::Relaxed),
            "memory_fallback_bytes":MEMORY_FALLBACK_BYTES.load(Ordering::Relaxed),
            "backing_create_failures":CREATE_FAILURES.load(Ordering::Relaxed)})
    }

    #[derive(Debug)]
    pub(crate) struct Store {
        inner: Mutex<Inner>,
        capacity: usize,
    }
    #[derive(Debug)]
    struct Inner {
        file: File,
        committed: u64,
        active: Vec<u8>,
        cache: VecDeque<(u64, Box<[u8]>)>,
        // File is declared first: its delete-on-close handle closes before the
        // directory is removed. A killed process still loses the backing file.
        #[cfg(windows)]
        _directory: TemporaryDirectory,
    }

    pub(super) fn current() -> Option<Arc<Store>> {
        static LIMIT: OnceLock<usize> = OnceLock::new();
        let limit = *LIMIT.get_or_init(|| {
            std::env::var("ROVE_PROPERTY_CACHE_MIB")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| (1..=16).contains(v))
                .unwrap_or(0)
                * 1024
                * 1024
        });
        if limit == 0 {
            return None;
        }
        CURRENT.with(|slot| {
            if let Some(store) = slot.borrow().upgrade() {
                return Some(store);
            }
            let store = match Store::new(limit) {
                Ok(store) => Arc::new(store),
                Err(_) => {
                    CREATE_FAILURES.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            };
            let mut registry = STORES
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            registry.retain(|entry| entry.strong_count() > 0);
            registry.push(Arc::downgrade(&store));
            *slot.borrow_mut() = Arc::downgrade(&store);
            Some(store)
        })
    }

    impl Store {
        pub(super) fn new(capacity: usize) -> io::Result<Self> {
            #[cfg(unix)]
            let file = {
                // mkstemp creates mode 0600; unlink before exposing the descriptor.
                // The OS reclaims it even if the client crashes or is terminated.
                let path = std::env::temp_dir().join("rove-properties-XXXXXX");
                let mut name = path.as_os_str().as_bytes().to_vec();
                name.push(0);
                let descriptor = unsafe { libc::mkstemp(name.as_mut_ptr().cast()) };
                if descriptor < 0 {
                    return Err(io::Error::last_os_error());
                }
                let file = unsafe { File::from_raw_fd(descriptor) };
                if unsafe { libc::unlink(name.as_ptr().cast()) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                // The backing must not be inherited by child tools.
                if unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                file
            };
            #[cfg(windows)]
            let (file, directory) = {
                let mut bytes = [0; 16];
                crate::entropy::fill(&mut bytes)?;
                let name: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                let path = std::env::temp_dir().join(format!("rove-properties-{name}"));
                drop(crate::windows_private::create_directory(&path)?);
                let directory = TemporaryDirectory(path);
                let file = crate::windows_private::create_file(&directory.0.join("data"), true)?;
                (file, directory)
            };
            Ok(Self {
                inner: Mutex::new(Inner {
                    file,
                    committed: 0,
                    active: Vec::with_capacity(PAGE),
                    cache: VecDeque::new(),
                    #[cfg(windows)]
                    _directory: directory,
                }),
                capacity: (capacity / PAGE).max(1) * PAGE,
            })
        }

        pub(super) fn append(&self, bytes: &[u8]) -> io::Result<u32> {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| io::Error::other("property storage lock poisoned"))?;
            let offset = inner.committed as usize + inner.active.len();
            if bytes.len() > MAX_FILE.saturating_sub(offset) {
                return Err(io::Error::other("property backing limit exceeded"));
            }
            let mut remaining = bytes;
            while !remaining.is_empty() {
                if inner.active.len() == PAGE {
                    write_all_at(&inner.file, &inner.active, inner.committed)?;
                    inner.committed += PAGE as u64;
                    inner.active.clear();
                }
                let take = (PAGE - inner.active.len()).min(remaining.len());
                inner.active.extend_from_slice(&remaining[..take]);
                remaining = &remaining[take..];
            }
            Ok(offset as u32)
        }

        pub(super) fn read(&self, offset: u32, length: u32) -> io::Result<Vec<u8>> {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| io::Error::other("property storage lock poisoned"))?;
            let end = u64::from(offset) + u64::from(length);
            if end > inner.committed + inner.active.len() as u64 {
                return Err(io::Error::other(
                    "property storage read exceeds committed range",
                ));
            }
            let mut output = Vec::with_capacity(length as usize);
            let mut position = u64::from(offset);
            while position < end {
                if position >= inner.committed {
                    let start = (position - inner.committed) as usize;
                    output
                        .extend_from_slice(&inner.active[start..start + (end - position) as usize]);
                    break;
                }
                let page = position / PAGE as u64 * PAGE as u64;
                let entry =
                    if let Some(index) = inner.cache.iter().position(|(key, _)| *key == page) {
                        inner
                            .cache
                            .remove(index)
                            .expect("existing property cache page")
                    } else {
                        // Evict before reading so a miss never retains capacity+1 pages.
                        while inner.cache.len() * PAGE >= self.capacity {
                            inner.cache.pop_front();
                        }
                        let mut data = vec![0; PAGE].into_boxed_slice();
                        read_exact_at(&inner.file, &mut data, page)?;
                        (page, data)
                    };
                let start = (position - page) as usize;
                let take = (PAGE - start).min((end - position) as usize);
                output.extend_from_slice(&entry.1[start..start + take]);
                position += take as u64;
                inner.cache.push_back(entry);
            }
            Ok(output)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn bounded_cache_preserves_cross_page_reads_and_concurrent_appends() {
            let store = Arc::new(Store::new(PAGE * 2).unwrap());
            let mut rows = Vec::new();
            for index in 0..80 {
                let bytes = (0..4097)
                    .map(|n| ((n * 29 + index) % 251) as u8)
                    .collect::<Vec<_>>();
                rows.push((store.append(&bytes).unwrap(), bytes));
            }
            let reader = store.clone();
            let worker = std::thread::spawn(move || {
                for (offset, bytes) in rows.iter().rev() {
                    assert_eq!(reader.read(*offset, bytes.len() as u32).unwrap(), *bytes);
                    assert!(reader.inner.lock().unwrap().cache.len() * PAGE <= PAGE * 2);
                }
            });
            for _ in 0..20 {
                store.append(&vec![254; 9000]).unwrap();
            }
            worker.join().unwrap();
            assert!(store.read(u32::MAX, 1).is_err());
            assert!(store.read(0, u32::MAX).is_err());
        }

        #[cfg(windows)]
        #[test]
        fn backing_is_private_and_removed_with_last_owner() {
            let store = Arc::new(Store::new(PAGE).unwrap());
            let directory = store.inner.lock().unwrap()._directory.0.clone();
            assert!(
                crate::windows_private::open_read(&directory.join("data"), MAX_FILE as u64).is_ok()
            );
            let owner = store.clone();
            drop(store);
            assert!(directory.join("data").exists());
            drop(owner);
            assert!(!directory.exists());
        }

        #[test]
        #[cfg(unix)]
        fn backing_is_private_unlinked_and_closes_with_last_owner() {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::MetadataExt;
            let store = Store::new(PAGE).unwrap();
            let inner = store.inner.lock().unwrap();
            let metadata = inner.file.metadata().unwrap();
            assert_eq!(metadata.nlink(), 0);
            assert_eq!(metadata.mode() & 0o777, 0o600);
            assert_ne!(
                unsafe { libc::fcntl(inner.file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
    }
}
